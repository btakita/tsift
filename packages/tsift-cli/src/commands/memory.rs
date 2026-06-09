use crate::cli::{MemoryCommand, MemoryProjectReadPolicy};
use crate::output::{OutputFormat, ToolEnvelopeSummary};
use crate::{envelope_metric, print_json_or_envelope, to_json_schema};
use anyhow::{Context, Result, bail};
use rusqlite::OptionalExtension;
use serde::Serialize;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};
use tsift_memgraphrag::{
    MemoryQueryPlan, derive_memory_ontology_graph, memory_graph_node_kinds, plan_memory_query,
    project_memory_into_graph_with_policy,
};
use tsift_memory::{
    AuthoredNodeKind, ClaudeMemImportPlan, MemoryBudget, MemoryBudgetGuardInput, MemoryEvent,
    MemoryEventKind, MemoryHandoffPlan, MemoryReadPolicy, MemoryStore, agent_doc_closeout_events,
    agent_doc_hook_contract, authored_node_projection, default_claude_mem_db_path,
    default_memory_db_path, guard_memory_handoff, import_claude_mem, inspect_claude_mem,
    memory_schema_sql, plan_capture_handoff,
};
use tsift_sqlite::{GraphStore, SqliteGraphStore};

const DEFAULT_CLAUDE_MEM_IMPORT_LIMIT: usize = 1000;

#[derive(Serialize)]
struct MemoryStatusReport {
    contract_version: String,
    schema_version: i64,
    memory_db: String,
    initialized: bool,
    event_count: Option<usize>,
    schema_tables: Vec<&'static str>,
    graph_node_kinds: Vec<&'static str>,
    hook_contract: Vec<tsift_memory::MemoryHookSpec>,
    claude_mem: ClaudeMemImportPlan,
    claude_mem_retirement: ClaudeMemRetirementGate,
    query_api: MemoryQueryPlan,
    next_commands: Vec<String>,
}

#[derive(Serialize)]
struct ClaudeMemRetirementGate {
    decision: &'static str,
    direct_reads_allowed: bool,
    rollback_until_normal_session_cycle: bool,
    conditions: Vec<ClaudeMemRetirementCondition>,
    rollback_commands: Vec<String>,
    next_commands: Vec<String>,
    notes: Vec<String>,
}

#[derive(Serialize)]
struct ClaudeMemRetirementCondition {
    name: &'static str,
    status: &'static str,
    evidence: String,
}

#[derive(Serialize)]
struct MemoryInitReport {
    contract_version: String,
    memory_db: String,
    schema_version: i64,
    event_count: usize,
    next_commands: Vec<String>,
}

#[derive(Serialize)]
struct MemoryCaptureReport {
    contract_version: String,
    memory_db: String,
    session_path: String,
    captured_events: usize,
    new_events: usize,
    event_ids: Vec<String>,
    event_kinds: Vec<String>,
    next_commands: Vec<String>,
}

struct MemoryBudgetGuardOptions<'a> {
    text: Option<&'a str>,
    file: Option<&'a Path>,
    byte_start: Option<usize>,
    byte_end: Option<usize>,
    source_ref: &'a str,
    payload_kind: &'a str,
    budget: MemoryBudget,
}

pub(crate) fn cmd_memory(command: MemoryCommand, format: OutputFormat) -> Result<()> {
    match command {
        MemoryCommand::Status {
            path,
            claude_mem_db,
            ..
        } => cmd_memory_status(&path, claude_mem_db.as_deref(), format),
        MemoryCommand::Init { path, .. } => cmd_memory_init(&path, format),
        MemoryCommand::ImportClaudeMem {
            path,
            db,
            limit,
            all,
            apply,
            ..
        } => {
            let limit_per_table = if all {
                None
            } else {
                Some(limit.unwrap_or(DEFAULT_CLAUDE_MEM_IMPORT_LIMIT))
            };
            cmd_memory_import_claude_mem(&path, db.as_deref(), limit_per_table, apply, format)
        }
        MemoryCommand::CaptureAgentDocCloseout {
            path,
            session_path,
            prompt_target,
            response_summary,
            commit_hash,
            session_check_status,
            ..
        } => cmd_memory_capture_agent_doc_closeout(
            &path,
            &session_path,
            &prompt_target,
            &response_summary,
            commit_hash.as_deref(),
            &session_check_status,
            format,
        ),
        MemoryCommand::HandoffPlan {
            text,
            budget_tokens,
            ..
        } => cmd_memory_handoff_plan(&text, budget_tokens, format),
        MemoryCommand::BudgetGuard {
            text,
            file,
            byte_start,
            byte_end,
            source_ref,
            payload_kind,
            budget_tokens,
            reserve_tokens,
            max_chunk_tokens,
            ..
        } => cmd_memory_budget_guard(
            MemoryBudgetGuardOptions {
                text: text.as_deref(),
                file: file.as_deref(),
                byte_start,
                byte_end,
                source_ref: &source_ref,
                payload_kind: &payload_kind,
                budget: MemoryBudget {
                    max_prompt_tokens: budget_tokens,
                    reserve_tokens,
                    max_event_tokens: max_chunk_tokens,
                },
            },
            format,
        ),
        MemoryCommand::QueryPlan {
            query,
            limit,
            max_tokens,
            ..
        } => cmd_memory_query_plan(&query, limit, max_tokens, format),
        MemoryCommand::ProjectGraph {
            path,
            graph_db,
            limit,
            read_policy,
            query,
            ..
        } => cmd_memory_project_graph(
            &path,
            graph_db.as_deref(),
            limit,
            read_policy,
            query.as_deref(),
            format,
        ),
        MemoryCommand::OntologyGraph { path, graph_db, .. } => {
            cmd_memory_ontology_graph(&path, graph_db.as_deref(), format)
        }
        MemoryCommand::Findings {
            path,
            kind,
            anchor,
            query,
            limit,
            graph_db,
            ..
        } => cmd_memory_findings(
            &path,
            &kind,
            anchor.as_deref(),
            query.as_deref(),
            limit,
            graph_db.as_deref(),
            format,
        ),
        MemoryCommand::FindingAdd {
            path,
            kind,
            text,
            anchor,
            confidence,
            session_id,
            graph_db,
            ..
        } => cmd_memory_finding_add(
            &path,
            &kind,
            &text,
            &anchor,
            confidence,
            session_id.as_deref(),
            graph_db.as_deref(),
            format,
        ),
    }
}

fn cmd_memory_status(
    path: &Path,
    claude_mem_db: Option<&Path>,
    format: OutputFormat,
) -> Result<()> {
    let memory_db = default_memory_db_path(path);
    let (event_count, imported_claude_mem_events) = if memory_db.exists() {
        let conn = rusqlite::Connection::open(&memory_db)
            .with_context(|| format!("open {}", memory_db.display()))?;
        let count: i64 =
            conn.query_row("SELECT COUNT(*) FROM memory_events", [], |row| row.get(0))?;
        let imported_count: i64 = conn.query_row(
            "SELECT COUNT(*) FROM memory_events WHERE imported_from = 'claude-mem'",
            [],
            |row| row.get(0),
        )?;
        (Some(count as usize), Some(imported_count as usize))
    } else {
        (None, None)
    };
    let claude_mem_path = resolve_claude_mem_path(claude_mem_db)?;
    let claude_mem = inspect_claude_mem(&claude_mem_path)?;
    let semantic_graph_rows = graph_semantic_row_count(path).map_err(|err| err.to_string());
    let claude_mem_retirement = build_claude_mem_retirement_gate(
        path,
        &claude_mem,
        imported_claude_mem_events,
        semantic_graph_rows,
    );
    let query_api = plan_memory_query("prompt too long", 10, 2000)?;
    let report = MemoryStatusReport {
        contract_version: tsift_memory::MEMORY_CONTRACT_VERSION.to_string(),
        schema_version: tsift_memory::MEMORY_SCHEMA_VERSION,
        memory_db: memory_db.display().to_string(),
        initialized: memory_db.exists(),
        event_count,
        schema_tables: vec![
            "memory_events",
            "memory_events_fts",
            "memory_internal_state",
            "memory_session_summaries",
            "memory_artifacts",
            "memory_tool_spans",
            "memory_embeddings",
            "memory_graph_links",
            "memory_import_runs",
        ],
        graph_node_kinds: memory_graph_node_kinds(),
        hook_contract: agent_doc_hook_contract(),
        claude_mem,
        claude_mem_retirement,
        query_api,
        next_commands: vec![
            format!("tsift memory init {}", path.display()),
            format!(
                "tsift graph-db --path {} --json related '<query>'",
                path.display()
            ),
            format!(
                "tsift memory import-claude-mem {} --all --apply --json",
                path.display()
            ),
            "tsift memory handoff-plan '<event text>' --budget-tokens 4096 --json".to_string(),
        ],
    };
    print_memory_report(
        &report,
        &format,
        "status",
        ToolEnvelopeSummary {
            text: "tsift-memory graph retrieval and fallback import readiness".to_string(),
            metrics: vec![
                envelope_metric("schema_version", report.schema_version),
                envelope_metric("initialized", report.initialized),
            ],
        },
        report.next_commands.clone(),
    )
}

fn build_claude_mem_retirement_gate(
    path: &Path,
    claude_mem: &ClaudeMemImportPlan,
    imported_claude_mem_events: Option<usize>,
    semantic_graph_rows: Result<Option<usize>, String>,
) -> ClaudeMemRetirementGate {
    let source_rows = claude_mem_source_rows(claude_mem);
    let full_import = if !claude_mem.exists {
        ClaudeMemRetirementCondition {
            name: "full_import",
            status: "pass",
            evidence: format!(
                "no claude-mem database found at {}; no source rows require migration",
                claude_mem.db_path
            ),
        }
    } else if !claude_mem.readable {
        ClaudeMemRetirementCondition {
            name: "full_import",
            status: "block",
            evidence: format!(
                "claude-mem database exists at {} but is not readable; inspect or pass --claude-mem-db",
                claude_mem.db_path
            ),
        }
    } else if source_rows == 0 {
        ClaudeMemRetirementCondition {
            name: "full_import",
            status: "pass",
            evidence: "claude-mem readable, but supported durable tables have zero rows"
                .to_string(),
        }
    } else {
        let imported = imported_claude_mem_events.unwrap_or_default();
        let status = if imported >= source_rows {
            "pass"
        } else {
            "block"
        };
        ClaudeMemRetirementCondition {
            name: "full_import",
            status,
            evidence: format!(
                "{imported}/{source_rows} supported claude-mem rows are present in .tsift/memory.db"
            ),
        }
    };

    let semantic_retrieval = match semantic_graph_rows {
        Ok(Some(rows)) if rows > 0 => ClaudeMemRetirementCondition {
            name: "semantic_retrieval",
            status: "pass",
            evidence: format!(
                ".tsift/graph.db has {rows} semantic_concept/semantic_entity row(s) for graph-db related retrieval"
            ),
        },
        Ok(Some(_)) => ClaudeMemRetirementCondition {
            name: "semantic_retrieval",
            status: "block",
            evidence: "graph.db exists but has no semantic_concept/semantic_entity rows; run summarize extract and graph-db refresh".to_string(),
        },
        Ok(None) => ClaudeMemRetirementCondition {
            name: "semantic_retrieval",
            status: "block",
            evidence: "graph.db is missing or not initialized; run graph-db refresh and prove graph-db related retrieval".to_string(),
        },
        Err(err) => ClaudeMemRetirementCondition {
            name: "semantic_retrieval",
            status: "block",
            evidence: format!(
                "graph semantic readiness could not be inspected; run graph-db status/doctor before retiring claude-mem: {err}"
            ),
        },
    };

    let parity_eval = ClaudeMemRetirementCondition {
        name: "parity_eval",
        status: "manual_required",
        evidence: "run `tsift dci-benchmark --fixture packages/tsift-cli/fixtures/memory-retrieval-eval.json --json` and require memory_retrieval_gate.decision=pass".to_string(),
    };
    let normal_session = ClaudeMemRetirementCondition {
        name: "normal_session_cycle",
        status: "manual_required",
        evidence: "keep rollback available until one normal agent-doc session cycle succeeds without direct claude-mem or /mem-search reads".to_string(),
    };

    ClaudeMemRetirementGate {
        decision: "hold",
        direct_reads_allowed: true,
        rollback_until_normal_session_cycle: true,
        conditions: vec![full_import, semantic_retrieval, parity_eval, normal_session],
        rollback_commands: vec![
            format!(
                "tsift memory import-claude-mem {} --all --apply --json",
                path.display()
            ),
            format!("tsift graph-db --path {} --json refresh", path.display()),
            format!(
                "tsift graph-db --path {} --json related '<query>'",
                path.display()
            ),
        ],
        next_commands: vec![
            format!(
                "tsift memory import-claude-mem {} --all --apply --json",
                path.display()
            ),
            format!("tsift graph-db --path {} --json refresh", path.display()),
            format!(
                "tsift graph-db --path {} --json related '<query>'",
                path.display()
            ),
            "tsift dci-benchmark --fixture packages/tsift-cli/fixtures/memory-retrieval-eval.json --json".to_string(),
        ],
        notes: vec![
            "direct claude-mem reads are retained as fallback until this gate is retired manually"
                .to_string(),
            "do not remove rollback commands until a normal session cycle proves no direct claude-mem dependency".to_string(),
        ],
    }
}

fn claude_mem_source_rows(plan: &ClaudeMemImportPlan) -> usize {
    plan.observations.rows + plan.session_summaries.rows + plan.user_prompts.rows
}

fn graph_semantic_row_count(path: &Path) -> Result<Option<usize>> {
    let graph_db = path.join(".tsift").join("graph.db");
    if !graph_db.exists() {
        return Ok(None);
    }
    let conn = rusqlite::Connection::open_with_flags(
        &graph_db,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_URI,
    )
    .with_context(|| format!("open {}", graph_db.display()))?;
    let table_exists: Option<i64> = conn
        .query_row(
            "SELECT 1 FROM sqlite_master WHERE type IN ('table','view') AND name = 'graph_nodes'",
            [],
            |row| row.get(0),
        )
        .optional()?;
    if table_exists.is_none() {
        return Ok(None);
    }
    let rows: i64 = conn.query_row(
        "SELECT COUNT(*) FROM graph_nodes WHERE kind IN ('semantic_concept', 'semantic_entity')",
        [],
        |row| row.get(0),
    )?;
    Ok(Some(rows as usize))
}

fn cmd_memory_init(path: &Path, format: OutputFormat) -> Result<()> {
    let memory_db = default_memory_db_path(path);
    let store = tsift_memory::MemoryStore::open_or_create(&memory_db)?;
    let report = MemoryInitReport {
        contract_version: tsift_memory::MEMORY_CONTRACT_VERSION.to_string(),
        memory_db: memory_db.display().to_string(),
        schema_version: tsift_memory::MEMORY_SCHEMA_VERSION,
        event_count: store.event_count()?,
        next_commands: vec![
            format!("tsift memory status {} --json", path.display()),
            format!(
                "tsift memory import-claude-mem {} --all --apply --json",
                path.display()
            ),
        ],
    };
    print_memory_report(
        &report,
        &format,
        "init",
        ToolEnvelopeSummary {
            text: "tsift-memory database initialized".to_string(),
            metrics: vec![
                envelope_metric("schema_version", report.schema_version),
                envelope_metric("event_count", report.event_count),
            ],
        },
        report.next_commands.clone(),
    )
}

fn cmd_memory_import_claude_mem(
    path: &Path,
    db: Option<&Path>,
    limit_per_table: Option<usize>,
    apply: bool,
    format: OutputFormat,
) -> Result<()> {
    let source = resolve_claude_mem_path(db)?;
    let target = default_memory_db_path(path);
    let report = import_claude_mem(&source, &target, limit_per_table, !apply)?;
    print_memory_report(
        &report,
        &format,
        "import-claude-mem",
        ToolEnvelopeSummary {
            text: if apply {
                "claude-mem import applied"
            } else {
                "claude-mem import planned"
            }
            .to_string(),
            metrics: vec![
                envelope_metric("source_rows", report.reconciliation.total_source_rows),
                envelope_metric("planned_events", report.planned_events),
                envelope_metric("imported_events", report.imported_events),
                envelope_metric("already_present_events", report.already_present_events),
                envelope_metric("complete", report.reconciliation.complete),
            ],
        },
        vec![
            format!("tsift memory status {} --json", path.display()),
            format!("tsift graph-db --path {} --json refresh", path.display()),
            format!(
                "tsift graph-db --path {} --json related '<query>'",
                path.display()
            ),
            format!(
                "tsift memory import-claude-mem {} --all --apply --json",
                path.display()
            ),
        ],
    )
}

fn cmd_memory_capture_agent_doc_closeout(
    path: &Path,
    session_path: &Path,
    prompt_target: &str,
    response_summary: &str,
    commit_hash: Option<&str>,
    session_check_status: &str,
    format: OutputFormat,
) -> Result<()> {
    let memory_db = default_memory_db_path(path);
    let store = MemoryStore::open_or_create(&memory_db)?;
    let before = store.event_count()?;
    let events = agent_doc_closeout_events(
        session_path,
        prompt_target,
        response_summary,
        commit_hash,
        session_check_status,
    );
    let mut event_ids = Vec::with_capacity(events.len());
    let mut event_kinds = Vec::with_capacity(events.len());
    for event in &events {
        event_ids.push(store.insert_event(event)?);
        event_kinds.push(event.kind.as_str().to_string());
    }
    let after = store.event_count()?;
    let report = MemoryCaptureReport {
        contract_version: tsift_memory::MEMORY_CONTRACT_VERSION.to_string(),
        memory_db: memory_db.display().to_string(),
        session_path: session_path.display().to_string(),
        captured_events: events.len(),
        new_events: after.saturating_sub(before),
        event_ids,
        event_kinds,
        next_commands: vec![
            format!("tsift memory status {} --json", path.display()),
            format!("tsift graph-db --path {} --json refresh", path.display()),
            format!(
                "tsift graph-db --path {} --json related '<query>'",
                path.display()
            ),
        ],
    };
    print_memory_report(
        &report,
        &format,
        "capture-agent-doc-closeout",
        ToolEnvelopeSummary {
            text: "agent-doc closeout captured into tsift-memory".to_string(),
            metrics: vec![
                envelope_metric("captured_events", report.captured_events),
                envelope_metric("new_events", report.new_events),
            ],
        },
        report.next_commands.clone(),
    )
}

fn cmd_memory_handoff_plan(text: &str, budget_tokens: usize, format: OutputFormat) -> Result<()> {
    let event = MemoryEvent::new(MemoryEventKind::ToolResultArtifact, "inline", text);
    let plan = plan_capture_handoff(&[event], MemoryBudget::new(budget_tokens));
    print_memory_report(
        &plan,
        &format,
        "handoff-plan",
        handoff_summary(&plan),
        plan.next_commands.clone(),
    )
}

fn cmd_memory_budget_guard(
    options: MemoryBudgetGuardOptions<'_>,
    format: OutputFormat,
) -> Result<()> {
    let (payload, resolved_source_ref) = match (options.text, options.file) {
        (Some(text), None) => (text.to_string(), options.source_ref.to_string()),
        (None, Some(file)) => {
            let full_payload = std::fs::read_to_string(file)
                .with_context(|| format!("read {}", file.display()))?;
            let payload = slice_payload(&full_payload, options.byte_start, options.byte_end)?;
            let source_ref = if options.source_ref == "inline" {
                file.display().to_string()
            } else {
                options.source_ref.to_string()
            };
            (payload, source_ref)
        }
        (None, None) => bail!("memory budget-guard requires --text or --file"),
        (Some(_), Some(_)) => bail!("memory budget-guard accepts only one of --text or --file"),
    };
    let report = guard_memory_handoff(
        MemoryBudgetGuardInput::new(resolved_source_ref, options.payload_kind, payload),
        options.budget,
    );
    print_memory_report(
        &report,
        &format,
        "budget-guard",
        ToolEnvelopeSummary {
            text: format!("memory budget guard {}", report.status),
            metrics: vec![
                envelope_metric("allowed", report.allowed),
                envelope_metric("estimated_tokens", report.estimated_tokens),
                envelope_metric("chunks", report.retryable_chunk_plan.len()),
            ],
        },
        report.next_commands.clone(),
    )
}

fn slice_payload(
    payload: &str,
    byte_start: Option<usize>,
    byte_end: Option<usize>,
) -> Result<String> {
    let start = byte_start.unwrap_or(0);
    let end = byte_end.unwrap_or(payload.len());
    if start > end || end > payload.len() {
        bail!(
            "invalid byte range {start}..{end} for payload with {} bytes",
            payload.len()
        );
    }
    if !payload.is_char_boundary(start) || !payload.is_char_boundary(end) {
        bail!("byte range {start}..{end} does not align with UTF-8 character boundaries");
    }
    Ok(payload[start..end].to_string())
}

fn cmd_memory_query_plan(
    query: &str,
    limit: usize,
    max_tokens: usize,
    format: OutputFormat,
) -> Result<()> {
    let plan = plan_memory_query(query, limit, max_tokens)?;
    print_memory_report(
        &plan,
        &format,
        "query-plan",
        ToolEnvelopeSummary {
            text: "tsift-memory query packet contract".to_string(),
            metrics: vec![
                envelope_metric("limit", limit),
                envelope_metric("candidate_limit", plan.candidate_limit),
                envelope_metric("max_tokens", max_tokens),
            ],
        },
        plan.next_commands.clone(),
    )
}

#[derive(Serialize)]
struct MemoryProjectGraphReport {
    contract_version: String,
    memory_db: String,
    graph_db: String,
    read_policy: MemoryReadPolicy,
    source_watermark: String,
    content_hash: String,
    events_available: usize,
    events_projected: usize,
    nodes_upserted: usize,
    edges_upserted: usize,
    graph_node_kinds: Vec<&'static str>,
    next_commands: Vec<String>,
}

fn cmd_memory_project_graph(
    path: &Path,
    graph_db_override: Option<&Path>,
    limit: usize,
    policy_arg: MemoryProjectReadPolicy,
    query: Option<&str>,
    format: OutputFormat,
) -> Result<()> {
    let memory_db = default_memory_db_path(path);
    let graph_db = graph_db_override
        .map(Path::to_path_buf)
        .unwrap_or_else(|| crate::graph_substrate_db_path(path, None));
    let read_policy = match policy_arg {
        MemoryProjectReadPolicy::RecentFirst => MemoryReadPolicy::recent_first(),
        MemoryProjectReadPolicy::OldestFirst => MemoryReadPolicy::oldest_first(),
        MemoryProjectReadPolicy::QueryRelevant => {
            let Some(query) = query.filter(|value| !value.trim().is_empty()) else {
                bail!("memory project-graph --read-policy=query-relevant requires --query");
            };
            MemoryReadPolicy::query_relevant(query)
        }
    };
    let graph_report =
        project_memory_into_graph_with_policy(&memory_db, &graph_db, limit, &read_policy)?;

    let next_commands = vec![
        "tsift graph-db --path . --json related '<query>'".to_string(),
        "tsift memory status . --json".to_string(),
    ];
    let report = MemoryProjectGraphReport {
        contract_version: tsift_memory::MEMORY_CONTRACT_VERSION.to_string(),
        memory_db: memory_db.display().to_string(),
        graph_db: graph_db.display().to_string(),
        read_policy: graph_report.read_policy,
        source_watermark: graph_report.source_watermark,
        content_hash: graph_report.content_hash,
        events_available: graph_report.events_available,
        events_projected: graph_report.events_projected,
        nodes_upserted: graph_report.nodes_upserted,
        edges_upserted: graph_report.edges_upserted,
        graph_node_kinds: memory_graph_node_kinds(),
        next_commands: next_commands.clone(),
    };
    print_memory_report(
        &report,
        &format,
        "project-graph",
        ToolEnvelopeSummary {
            text: "memory events projected into shared graph store".to_string(),
            metrics: vec![
                envelope_metric("available", graph_report.events_available),
                envelope_metric("events", graph_report.events_projected),
                envelope_metric("nodes", graph_report.nodes_upserted),
                envelope_metric("edges", graph_report.edges_upserted),
            ],
        },
        next_commands,
    )
}

#[derive(Serialize)]
struct MemoryOntologyReport {
    contract_version: String,
    graph_db: String,
    type_nodes: usize,
    relations: usize,
    next_commands: Vec<String>,
}

/// Derive the Semantic Ontology Graph layer (#memgraphrag-ont) from the shared
/// graph store and upsert it back so node/edge KIND types + permitted relations
/// are queryable alongside instances.
fn cmd_memory_ontology_graph(
    path: &Path,
    graph_db_override: Option<&Path>,
    format: OutputFormat,
) -> Result<()> {
    let graph_db = graph_db_override
        .map(Path::to_path_buf)
        .unwrap_or_else(|| crate::graph_substrate_db_path(path, None));
    let ontology = derive_memory_ontology_graph(&graph_db)?;

    let next_commands = vec![
        "tsift graph-db --path . --json related 'ontology_type'".to_string(),
        "tsift memory finding-add --text '<finding>' --anchor '<symbol-handle>'".to_string(),
    ];
    let report = MemoryOntologyReport {
        contract_version: tsift_memory::MEMORY_CONTRACT_VERSION.to_string(),
        graph_db: graph_db.display().to_string(),
        type_nodes: ontology.type_nodes,
        relations: ontology.relations,
        next_commands: next_commands.clone(),
    };
    print_memory_report(
        &report,
        &format,
        "ontology-graph",
        ToolEnvelopeSummary {
            text: "derived semantic ontology graph layer".to_string(),
            metrics: vec![
                envelope_metric("type_nodes", ontology.type_nodes),
                envelope_metric("relations", ontology.relations),
            ],
        },
        next_commands,
    )
}

#[derive(Serialize)]
struct FindingRecord {
    id: String,
    kind: String,
    text: String,
    anchor_handle: String,
    confidence: f64,
    observed_at_unix: i64,
}

#[derive(Serialize)]
struct MemoryFindingsReport {
    contract_version: String,
    graph_db: String,
    count: usize,
    findings: Vec<FindingRecord>,
    next_commands: Vec<String>,
}

/// Query authored finding/decision/note nodes from the graph (#trt1 retrieval),
/// newest first by `observed_at_unix`, with optional kind/anchor/lexical filters.
fn query_findings(
    graph_db: &Path,
    kind: &str,
    anchor: Option<&str>,
    query: Option<&str>,
    limit: usize,
) -> Result<Vec<FindingRecord>> {
    let kinds: Vec<&str> = match kind {
        "all" => vec!["finding", "decision", "note"],
        "finding" | "decision" | "note" => vec![kind],
        other => bail!("unsupported finding kind `{other}` (expected finding|decision|note|all)"),
    };
    if !graph_db.exists() {
        return Ok(Vec::new());
    }
    let store = SqliteGraphStore::open(graph_db)
        .with_context(|| format!("open graph store {}", graph_db.display()))?;
    let query_lower = query.map(|q| q.to_lowercase());
    let mut records = Vec::new();
    for kind in kinds {
        for node in store.nodes_by_kind(kind)? {
            let anchor_handle = node
                .properties
                .get("anchor_handle")
                .cloned()
                .unwrap_or_default();
            if let Some(want) = anchor
                && anchor_handle != want
            {
                continue;
            }
            let text = node
                .properties
                .get("text")
                .cloned()
                .unwrap_or_else(|| node.label.clone());
            if let Some(needle) = &query_lower
                && !text.to_lowercase().contains(needle.as_str())
            {
                continue;
            }
            let confidence = node
                .properties
                .get("confidence")
                .and_then(|v| v.parse::<f64>().ok())
                .unwrap_or(1.0);
            let observed_at_unix = node
                .properties
                .get("observed_at_unix")
                .and_then(|v| v.parse::<i64>().ok())
                .unwrap_or(0);
            records.push(FindingRecord {
                id: node.id,
                kind: kind.to_string(),
                text,
                anchor_handle,
                confidence,
                observed_at_unix,
            });
        }
    }
    records.sort_by(|a, b| {
        b.observed_at_unix
            .cmp(&a.observed_at_unix)
            .then(a.id.cmp(&b.id))
    });
    records.truncate(limit);
    Ok(records)
}

fn cmd_memory_findings(
    path: &Path,
    kind: &str,
    anchor: Option<&str>,
    query: Option<&str>,
    limit: usize,
    graph_db_override: Option<&Path>,
    format: OutputFormat,
) -> Result<()> {
    let graph_db = graph_db_override
        .map(Path::to_path_buf)
        .unwrap_or_else(|| crate::graph_substrate_db_path(path, None));
    let findings = query_findings(&graph_db, kind, anchor, query, limit)?;
    let count = findings.len();
    let next_commands = vec![
        "tsift memory finding-add --text '<finding>' --anchor '<symbol-handle>'".to_string(),
        "tsift memory ontology-graph . --json".to_string(),
    ];
    let report = MemoryFindingsReport {
        contract_version: tsift_memory::MEMORY_CONTRACT_VERSION.to_string(),
        graph_db: graph_db.display().to_string(),
        count,
        findings,
        next_commands: next_commands.clone(),
    };
    print_memory_report(
        &report,
        &format,
        "findings",
        ToolEnvelopeSummary {
            text: format!("{count} authored finding(s)"),
            metrics: vec![envelope_metric("count", count)],
        },
        next_commands,
    )
}

#[derive(Serialize)]
struct MemoryFindingReport {
    contract_version: String,
    graph_db: String,
    node_kind: String,
    anchor_handle: String,
    anchor_resolved: bool,
    confidence: f64,
    observed_at_unix: i64,
    next_commands: Vec<String>,
}

/// Core of `#trt1`: build an authored node projection and upsert it into the
/// graph store. The `annotates` edge requires the anchor node to exist (FK on
/// graph_edges); if the anchor symbol is not in the graph yet, keep the authored
/// node (it still carries `anchor_handle`) and defer the edge until the anchor
/// lands. Returns `(anchor_resolved, nodes_upserted, edges_upserted)`.
#[allow(clippy::too_many_arguments)]
fn add_authored_node_to_graph(
    graph_db: &Path,
    kind: AuthoredNodeKind,
    text: &str,
    anchor: &str,
    confidence: f64,
    observed_at_unix: i64,
    session_id: Option<&str>,
) -> Result<(bool, usize, usize)> {
    let mut projection =
        authored_node_projection(kind, text, anchor, confidence, observed_at_unix, session_id);
    if let Some(parent) = graph_db.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("create graph db dir {}", parent.display()))?;
    }
    let mut store = SqliteGraphStore::open(graph_db)
        .with_context(|| format!("open graph store {}", graph_db.display()))?;
    let anchor_resolved = store.node(anchor)?.is_some();
    if !anchor_resolved {
        projection.edges.clear();
    }
    store.upsert_projection(&projection)?;
    Ok((
        anchor_resolved,
        projection.nodes.len(),
        projection.edges.len(),
    ))
}

/// Add an authored finding/decision/note node anchored to a symbol handle (#trt1).
#[allow(clippy::too_many_arguments)]
fn cmd_memory_finding_add(
    path: &Path,
    kind: &str,
    text: &str,
    anchor: &str,
    confidence: f64,
    session_id: Option<&str>,
    graph_db_override: Option<&Path>,
    format: OutputFormat,
) -> Result<()> {
    let authored_kind = AuthoredNodeKind::parse(kind)?;
    let observed_at_unix = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    let graph_db = graph_db_override
        .map(Path::to_path_buf)
        .unwrap_or_else(|| crate::graph_substrate_db_path(path, None));
    let (anchor_resolved, nodes_upserted, edges_upserted) = add_authored_node_to_graph(
        &graph_db,
        authored_kind,
        text,
        anchor,
        confidence,
        observed_at_unix,
        session_id,
    )?;

    let next_commands = vec![
        "tsift memory ontology-graph . --json".to_string(),
        format!("tsift graph-db --path . --json related '{anchor}'"),
    ];
    let report = MemoryFindingReport {
        contract_version: tsift_memory::MEMORY_CONTRACT_VERSION.to_string(),
        graph_db: graph_db.display().to_string(),
        node_kind: authored_kind.as_str().to_string(),
        anchor_handle: anchor.to_string(),
        anchor_resolved,
        confidence: confidence.clamp(0.0, 1.0),
        observed_at_unix,
        next_commands: next_commands.clone(),
    };
    print_memory_report(
        &report,
        &format,
        "finding-add",
        ToolEnvelopeSummary {
            text: format!(
                "authored {} node anchored to {anchor}",
                authored_kind.as_str()
            ),
            metrics: vec![
                envelope_metric("nodes", nodes_upserted),
                envelope_metric("edges", edges_upserted),
            ],
        },
        next_commands,
    )
}

fn resolve_claude_mem_path(explicit: Option<&Path>) -> Result<PathBuf> {
    explicit
        .map(Path::to_path_buf)
        .or_else(default_claude_mem_db_path)
        .context("could not infer claude-mem db path; pass --db <path>")
}

fn handoff_summary(plan: &MemoryHandoffPlan) -> ToolEnvelopeSummary {
    ToolEnvelopeSummary {
        text: format!("memory handoff {}", plan.status),
        metrics: vec![
            envelope_metric("included_tokens", plan.estimated_included_tokens),
            envelope_metric("deferred_events", plan.deferred_events.len()),
        ],
    }
}

fn print_memory_report<T: Serialize>(
    report: &T,
    format: &OutputFormat,
    view: &str,
    summary: ToolEnvelopeSummary,
    follow_up: Vec<String>,
) -> Result<()> {
    if format.json_output {
        print_json_or_envelope(report, format, "memory", view, summary, false, follow_up)
    } else {
        println!(
            "{}",
            to_json_schema(report, format.pretty, false, false, false)?
        );
        Ok(())
    }
}

#[allow(dead_code)]
fn _schema_sql_for_tests() -> &'static str {
    memory_schema_sql()
}

#[cfg(test)]
mod tests {
    use super::*;
    use rusqlite::Connection;
    use tempfile::TempDir;
    use tsift_memory::MemoryStore;

    #[test]
    fn project_memory_into_graph_persists_memory_nodes() {
        let dir = TempDir::new().unwrap();
        let root = dir.path();
        let memory_db = default_memory_db_path(root);
        std::fs::create_dir_all(memory_db.parent().unwrap()).unwrap();

        let store = MemoryStore::open_or_create(&memory_db).unwrap();
        let mut prompt = MemoryEvent::new(
            MemoryEventKind::PromptTarget,
            "session.md",
            "run the gated backlog items",
        );
        prompt.session_id = Some("sess-1".to_string());
        prompt.observed_at_unix = Some(1_700_000_000);
        let mut response = MemoryEvent::new(
            MemoryEventKind::ResponseSummary,
            "session.md",
            "decay weighted retrieval shipped",
        );
        response.session_id = Some("sess-1".to_string());
        response.observed_at_unix = Some(1_700_000_100);
        store.insert_event(&prompt).unwrap();
        store.insert_event(&response).unwrap();

        let graph_db = root.join(".tsift").join("graph.db");
        let report =
            tsift_memgraphrag::project_memory_into_graph(&memory_db, &graph_db, 100).unwrap();
        assert_eq!(report.events_projected, 2);
        assert_eq!(report.events_available, 2);
        assert_eq!(report.read_policy.order.as_str(), "recent_first");
        assert!(!report.source_watermark.is_empty());
        assert!(!report.content_hash.is_empty());
        assert!(
            report.nodes_upserted >= 4,
            "two events + one session node + projection metadata, got {}",
            report.nodes_upserted
        );
        assert!(
            report.edges_upserted >= 2,
            "session records each event, got {}",
            report.edges_upserted
        );

        // The unified graph store persists memory nodes alongside code symbols.
        let conn = Connection::open(&graph_db).unwrap();
        let memory_events: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM graph_nodes WHERE kind = 'memory_event'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(memory_events, 2);
        let sessions: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM graph_nodes WHERE kind = 'memory_session'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(sessions, 1);
        let projection_metadata: (String, String, String) = conn
            .query_row(
                "SELECT json_extract(properties_json, '$.read_policy'),
                        json_extract(properties_json, '$.source_watermark'),
                        json_extract(properties_json, '$.content_hash')
                 FROM graph_nodes
                 WHERE id = 'memory_projection:tsift-memory'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .unwrap();
        assert_eq!(projection_metadata.0, "recent_first");
        assert_eq!(projection_metadata.1, report.source_watermark);
        assert_eq!(projection_metadata.2, report.content_hash);
    }

    #[test]
    fn finding_add_defers_edge_when_anchor_missing_then_resolves() {
        use tsift_core::GraphNode;
        let dir = TempDir::new().unwrap();
        let graph_db = dir.path().join("graph.db");

        // Anchor not yet in the graph: node is added, edge deferred.
        let (resolved, nodes, edges) = add_authored_node_to_graph(
            &graph_db,
            AuthoredNodeKind::Finding,
            "decay ranking lives in rank_memory_events",
            "symbol:rank_memory_events",
            0.8,
            1_700_000_000,
            None,
        )
        .unwrap();
        assert!(!resolved);
        assert_eq!(nodes, 1);
        assert_eq!(edges, 0); // edge deferred (cleared) while anchor is absent

        let conn = Connection::open(&graph_db).unwrap();
        let findings: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM graph_nodes WHERE kind = 'finding'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(findings, 1);
        let annotates: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM graph_edges WHERE kind = 'annotates'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(annotates, 0, "edge deferred while anchor absent");

        // Now the anchor symbol exists: re-adding resolves the annotates edge.
        {
            let mut store = SqliteGraphStore::open(&graph_db).unwrap();
            let seed = tsift_core::GraphProjection {
                nodes: vec![GraphNode::new(
                    "symbol:rank_memory_events",
                    "function",
                    "rank_memory_events",
                )],
                edges: vec![],
            };
            store.upsert_projection(&seed).unwrap();
        }
        let (resolved2, _, _) = add_authored_node_to_graph(
            &graph_db,
            AuthoredNodeKind::Finding,
            "decay ranking lives in rank_memory_events",
            "symbol:rank_memory_events",
            0.8,
            1_700_000_100,
            None,
        )
        .unwrap();
        assert!(resolved2);
        let conn2 = Connection::open(&graph_db).unwrap();
        let annotates2: i64 = conn2
            .query_row(
                "SELECT COUNT(*) FROM graph_edges WHERE kind = 'annotates'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(annotates2, 1, "edge materializes once anchor exists");
    }

    #[test]
    fn query_findings_filters_and_orders_newest_first() {
        let dir = TempDir::new().unwrap();
        let graph_db = dir.path().join("graph.db");
        add_authored_node_to_graph(
            &graph_db,
            AuthoredNodeKind::Finding,
            "decay ranking is shipped",
            "symbol:a",
            0.9,
            1_000,
            None,
        )
        .unwrap();
        add_authored_node_to_graph(
            &graph_db,
            AuthoredNodeKind::Decision,
            "ontology source is NodeKind enums",
            "symbol:b",
            0.8,
            2_000,
            None,
        )
        .unwrap();
        add_authored_node_to_graph(
            &graph_db,
            AuthoredNodeKind::Note,
            "decay note for symbol a",
            "symbol:a",
            0.5,
            3_000,
            None,
        )
        .unwrap();

        // All kinds, newest first.
        let all = query_findings(&graph_db, "all", None, None, 20).unwrap();
        assert_eq!(all.len(), 3);
        assert_eq!(all[0].observed_at_unix, 3_000);
        assert_eq!(all[2].observed_at_unix, 1_000);

        // Kind filter.
        let decisions = query_findings(&graph_db, "decision", None, None, 20).unwrap();
        assert_eq!(decisions.len(), 1);
        assert_eq!(decisions[0].kind, "decision");

        // Anchor filter.
        let anchored = query_findings(&graph_db, "all", Some("symbol:a"), None, 20).unwrap();
        assert_eq!(anchored.len(), 2);
        assert!(anchored.iter().all(|f| f.anchor_handle == "symbol:a"));

        // Lexical filter + limit.
        let decay = query_findings(&graph_db, "all", None, Some("decay"), 1).unwrap();
        assert_eq!(decay.len(), 1);
        assert!(decay[0].text.to_lowercase().contains("decay"));

        // Missing graph db -> empty, no error.
        assert!(
            query_findings(
                dir.path().join("missing.db").as_path(),
                "all",
                None,
                None,
                5
            )
            .unwrap()
            .is_empty()
        );
    }
}
