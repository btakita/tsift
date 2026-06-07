use crate::cli::MemoryCommand;
use crate::output::{OutputFormat, ToolEnvelopeSummary};
use crate::{envelope_metric, print_json_or_envelope, to_json_schema};
use anyhow::{Context, Result, bail};
use rusqlite::OptionalExtension;
use serde::Serialize;
use std::path::{Path, PathBuf};
use tsift_memory::{
    ClaudeMemImportPlan, MemoryBudget, MemoryBudgetGuardInput, MemoryEvent, MemoryEventKind,
    MemoryHandoffPlan, MemoryQueryPlan, MemoryStore, agent_doc_closeout_events,
    agent_doc_hook_contract, default_claude_mem_db_path, default_memory_db_path,
    guard_memory_handoff, import_claude_mem, inspect_claude_mem, memory_graph_node_kinds,
    memory_schema_sql, plan_capture_handoff, plan_memory_query, project_memory_events,
    read_memory_events,
};
use tsift_sqlite::SqliteGraphStore;

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
            ..
        } => cmd_memory_project_graph(&path, graph_db.as_deref(), limit, format),
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
    events_projected: usize,
    nodes_upserted: usize,
    edges_upserted: usize,
    graph_node_kinds: Vec<&'static str>,
    next_commands: Vec<String>,
}

/// Project stored memory events into the shared code graph store (#memgraphrag2).
/// Memory `memory_session`/`memory_event` nodes become queryable alongside code
/// symbols through the same `graph-db` retrieval surface.
/// Core of `#memgraphrag2`: read memory events, project them, and upsert into the
/// shared graph store. Returns `(events_projected, nodes_upserted, edges_upserted)`.
fn project_memory_into_graph(
    memory_db: &Path,
    graph_db: &Path,
    limit: usize,
) -> Result<(usize, usize, usize)> {
    let events = read_memory_events(memory_db, limit)?;
    let projection = project_memory_events(&events);
    let nodes = projection.nodes.len();
    let edges = projection.edges.len();
    if let Some(parent) = graph_db.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("create graph db dir {}", parent.display()))?;
    }
    let mut store = SqliteGraphStore::open(graph_db)
        .with_context(|| format!("open graph store {}", graph_db.display()))?;
    store.upsert_projection(&projection)?;
    Ok((events.len(), nodes, edges))
}

fn cmd_memory_project_graph(
    path: &Path,
    graph_db_override: Option<&Path>,
    limit: usize,
    format: OutputFormat,
) -> Result<()> {
    let memory_db = default_memory_db_path(path);
    let graph_db = graph_db_override
        .map(Path::to_path_buf)
        .unwrap_or_else(|| crate::graph_substrate_db_path(path, None));
    let (events_projected, nodes_upserted, edges_upserted) =
        project_memory_into_graph(&memory_db, &graph_db, limit)?;

    let next_commands = vec![
        "tsift graph-db --path . --json related '<query>'".to_string(),
        "tsift memory status . --json".to_string(),
    ];
    let report = MemoryProjectGraphReport {
        contract_version: tsift_memory::MEMORY_CONTRACT_VERSION.to_string(),
        memory_db: memory_db.display().to_string(),
        graph_db: graph_db.display().to_string(),
        events_projected,
        nodes_upserted,
        edges_upserted,
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
                envelope_metric("events", events_projected),
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
        println!("{}", to_json_schema(report, format.pretty, false, false, false)?);
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
        let (events, nodes, edges) =
            project_memory_into_graph(&memory_db, &graph_db, 100).unwrap();
        assert_eq!(events, 2);
        assert!(nodes >= 3, "two events + one session node, got {nodes}");
        assert!(edges >= 2, "session records each event, got {edges}");

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
    }
}
