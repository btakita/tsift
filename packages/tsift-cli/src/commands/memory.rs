use crate::cli::MemoryCommand;
use crate::output::{OutputFormat, ToolEnvelopeSummary};
use crate::{envelope_metric, print_json_or_envelope, to_json_schema};
use anyhow::{Context, Result};
use serde::Serialize;
use std::path::{Path, PathBuf};
use tsift_memory::{
    ClaudeMemImportPlan, MemoryBudget, MemoryEvent, MemoryEventKind, MemoryHandoffPlan,
    MemoryQueryPlan, agent_doc_hook_contract, default_claude_mem_db_path, default_memory_db_path,
    import_claude_mem, inspect_claude_mem, memory_graph_node_kinds, memory_schema_sql,
    plan_capture_handoff, plan_memory_query,
};

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
    query_api: MemoryQueryPlan,
    next_commands: Vec<String>,
}

#[derive(Serialize)]
struct MemoryInitReport {
    contract_version: String,
    memory_db: String,
    schema_version: i64,
    event_count: usize,
    next_commands: Vec<String>,
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
            apply,
            ..
        } => cmd_memory_import_claude_mem(&path, db.as_deref(), limit, apply, format),
        MemoryCommand::HandoffPlan {
            text,
            budget_tokens,
            ..
        } => cmd_memory_handoff_plan(&text, budget_tokens, format),
        MemoryCommand::QueryPlan {
            query,
            limit,
            max_tokens,
            ..
        } => cmd_memory_query_plan(&query, limit, max_tokens, format),
    }
}

fn cmd_memory_status(
    path: &Path,
    claude_mem_db: Option<&Path>,
    format: OutputFormat,
) -> Result<()> {
    let memory_db = default_memory_db_path(path);
    let event_count = if memory_db.exists() {
        let conn = rusqlite::Connection::open(&memory_db)
            .with_context(|| format!("open {}", memory_db.display()))?;
        let count: i64 =
            conn.query_row("SELECT COUNT(*) FROM memory_events", [], |row| row.get(0))?;
        Some(count as usize)
    } else {
        None
    };
    let claude_mem_path = resolve_claude_mem_path(claude_mem_db)?;
    let claude_mem = inspect_claude_mem(&claude_mem_path)?;
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
        query_api,
        next_commands: vec![
            format!("tsift memory init {}", path.display()),
            format!(
                "tsift memory import-claude-mem {} --apply --json",
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
            text: "tsift-memory schema and import readiness".to_string(),
            metrics: vec![
                envelope_metric("schema_version", report.schema_version),
                envelope_metric("initialized", report.initialized),
            ],
        },
        report.next_commands.clone(),
    )
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
                "tsift memory import-claude-mem {} --apply --json",
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
    limit: usize,
    apply: bool,
    format: OutputFormat,
) -> Result<()> {
    let source = resolve_claude_mem_path(db)?;
    let target = default_memory_db_path(path);
    let report = import_claude_mem(&source, &target, limit, !apply)?;
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
                envelope_metric("planned_events", report.planned_events),
                envelope_metric("imported_events", report.imported_events),
            ],
        },
        vec![
            format!("tsift memory status {} --json", path.display()),
            "tsift graph-db --path . refresh --json".to_string(),
        ],
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
        println!("{}", to_json_schema(report, format.pretty, false, false)?);
        Ok(())
    }
}

#[allow(dead_code)]
fn _schema_sql_for_tests() -> &'static str {
    memory_schema_sql()
}
