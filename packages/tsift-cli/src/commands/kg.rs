//! `tsift kg` — local Knowledge Graph extraction via a lazy local model (#lmlazy).
//!
//! The `extract` subcommand reads source text, resolves an Ollama-served model
//! (by explicit `--model` tag or by `--profile`), runs it through the shared
//! `tsift-kg` pipeline, and either prints a human summary or upserts the
//! resulting `GraphProjection` into a `.tsift/graph.db`.
//!
//! `status` / `unload` / `smoke` complete the operational surface required by
//! `specs/local-kg-model.md` line 31-32 (#kgcliext).
use anyhow::{Context, Result, bail};
use serde::Serialize;
use std::collections::BTreeMap;
use std::io::Read;
use std::path::{Path, PathBuf};

use tsift_core::GraphStore;
use tsift_kg::{
    ChunkingConfig, KgInputDocument, KgInputKind, OllamaKgExtractor, extract_documents_to_projection,
    upsert_kg_projection_sqlite,
};
use tsift_local_model::profile_by_id;

/// Default extractor profile when neither `--model` nor `--profile` is given.
/// Picked because it is the smallest GPU-resident extractor served by Ollama in
/// the default profile set.
const DEFAULT_PROFILE_ID: &str = "qwen3-32b-q4-ollama";

/// Default `.tsift/graph.db` location for `tsift kg status` when `--graph-db`
/// is not supplied. Resolves against the current working directory.
const DEFAULT_GRAPH_DB_RELATIVE: &str = ".tsift/graph.db";

/// Sample text used by `tsift kg smoke` to exercise the KG pipeline
/// end-to-end against a live Ollama server.
const SMOKE_SAMPLE_TEXT: &str = "\
The tsift local-model crate owns GPU probing and the cooperative lease registry. \
tsift-kg consumes the lease registry to serialize extractor runs on a single GPU. \
OllamaKgExtractor posts to /api/chat with a structured-output JSON schema.";
const SMOKE_SOURCE_REF: &str = "tsift-kg-smoke";

pub(crate) fn cmd_kg_extract(
    profile: Option<String>,
    model: Option<String>,
    host: Option<String>,
    input: Option<PathBuf>,
    source_ref: Option<String>,
    graph_db: Option<PathBuf>,
    json: bool,
) -> Result<()> {
    let (text, default_source_ref) = read_input(input.as_deref())?;
    if text.trim().is_empty() {
        bail!("no source text to extract — provide --input <file> or pipe text on stdin");
    }
    let source_ref = source_ref.unwrap_or(default_source_ref);

    let resolved_model = resolve_model_tag(profile.as_deref(), model.as_deref())?;
    let extractor = match host.as_deref() {
        Some(host) => OllamaKgExtractor::new(&resolved_model).with_host(host),
        None => OllamaKgExtractor::new(&resolved_model),
    };

    let document = KgInputDocument::new(KgInputKind::Source, &source_ref, &text);
    let report = extract_documents_to_projection(&[document], &extractor, ChunkingConfig::default())
        .context("KG extraction pipeline failed")?;

    let entity_count: usize = report.projection.nodes.len();
    let relation_count: usize = report.projection.edges.len();

    let upsert = if let Some(graph_db) = graph_db.as_deref() {
        Some(upsert_kg_projection_sqlite(graph_db, &report.projection).context(
            format!("upserting KG projection into {}", graph_db.display()),
        )?)
    } else {
        None
    };

    if json {
        let payload = serde_json::json!({
            "provider_id": report.extracted_chunks.first().map(|c| c.chunk.id.as_str()).unwrap_or(""),
            "model": extractor.model(),
            "host": extractor.host(),
            "source_ref": source_ref,
            "chunks": report.chunks.len(),
            "entities": entity_count,
            "relations": relation_count,
            "upsert": upsert,
        });
        println!("{}", serde_json::to_string_pretty(&payload)?);
    } else {
        println!("KG extraction");
        println!("Model: {} ({})", extractor.model(), extractor.host());
        println!(
            "Chunks: {}  Entities: {}  Relations: {}",
            report.chunks.len(),
            entity_count,
            relation_count
        );
        for node in report.projection.nodes.iter().take(20) {
            println!("  - {} [{}]", node.label, node.kind);
        }
        if report.projection.nodes.len() > 20 {
            println!("  ... ({} more)", report.projection.nodes.len() - 20);
        }
        if let Some(graph_db) = graph_db.as_deref() {
            println!("Upserted into: {}", graph_db.display());
        }
    }
    Ok(())
}

fn read_input(input: Option<&Path>) -> Result<(String, String)> {
    match input {
        Some(path) => {
            let text = std::fs::read_to_string(path)
                .with_context(|| format!("reading input file {}", path.display()))?;
            Ok((text, path.display().to_string()))
        }
        None => {
            let mut text = String::new();
            std::io::stdin()
                .read_to_string(&mut text)
                .context("reading source text from stdin")?;
            Ok((text, "stdin".to_string()))
        }
    }
}

/// Resolve the Ollama model tag. `--model` wins; otherwise look up the profile's
/// `model_ref`; otherwise default to `DEFAULT_PROFILE_ID`'s tag.
fn resolve_model_tag(profile: Option<&str>, model: Option<&str>) -> Result<String> {
    if let Some(model) = model {
        return Ok(model.to_string());
    }
    let profile_id = profile.unwrap_or(DEFAULT_PROFILE_ID);
    let profile = profile_by_id(profile_id).ok_or_else(|| {
        anyhow::anyhow!(
            "unknown tsift local-model profile `{profile_id}`. \
             Use --model <ollama-tag> or one of: {}",
            list_profile_ids().join(", ")
        )
    })?;
    Ok(profile.model_ref.to_string())
}

fn list_profile_ids() -> Vec<String> {
    tsift_local_model::default_model_profiles()
        .into_iter()
        .map(|p| p.id.to_string())
        .collect()
}

// =============================================================================
// status (#kgcliext — spec local-kg-model.md line 31-32)
// =============================================================================

#[derive(Debug, Serialize)]
struct KgStatusReport {
    graph_db: String,
    exists: bool,
    total_nodes: usize,
    total_edges: usize,
    nodes_by_kind: BTreeMap<String, usize>,
    edges_by_kind: BTreeMap<String, usize>,
}

pub(crate) fn cmd_kg_status(graph_db: Option<PathBuf>, json: bool) -> Result<()> {
    let db_path = graph_db
        .unwrap_or_else(|| PathBuf::from(DEFAULT_GRAPH_DB_RELATIVE));
    let report = build_kg_status_report(&db_path)?;
    emit_status(&report, json);
    Ok(())
}

fn build_kg_status_report(db_path: &Path) -> Result<KgStatusReport> {
    let db_display = db_path.display().to_string();
    if !db_path.exists() {
        return Ok(KgStatusReport {
            graph_db: db_display,
            exists: false,
            total_nodes: 0,
            total_edges: 0,
            nodes_by_kind: BTreeMap::new(),
            edges_by_kind: BTreeMap::new(),
        });
    }

    let store = tsift_sqlite::SqliteGraphStore::open_read_only_resilient(db_path)
        .with_context(|| format!("opening graph.db read-only at {}", db_path.display()))?;
    let nodes = store
        .all_nodes()
        .with_context(|| "reading graph_nodes for kg status")?;
    let edges = store
        .all_edges()
        .with_context(|| "reading graph_edges for kg status")?;

    let mut nodes_by_kind = BTreeMap::new();
    for node in &nodes {
        *nodes_by_kind.entry(node.kind.clone()).or_insert(0) += 1;
    }
    let mut edges_by_kind = BTreeMap::new();
    for edge in &edges {
        *edges_by_kind.entry(edge.kind.clone()).or_insert(0) += 1;
    }

    Ok(KgStatusReport {
        graph_db: db_display,
        exists: true,
        total_nodes: nodes.len(),
        total_edges: edges.len(),
        nodes_by_kind,
        edges_by_kind,
    })
}

fn emit_status(report: &KgStatusReport, json: bool) {
    if json {
        println!("{}", serde_json::to_string_pretty(report).unwrap_or_else(|_| "{}".into()));
        return;
    }
    println!("KG status");
    println!("Graph DB: {}", report.graph_db);
    if !report.exists {
        println!("  (no graph.db at this path — run `tsift kg extract --graph-db <path>` to populate)");
        return;
    }
    println!(
        "Nodes: {}   Edges: {}",
        report.total_nodes, report.total_edges
    );
    if !report.nodes_by_kind.is_empty() {
        println!("Nodes by kind (top 10):");
        let mut node_kinds: Vec<_> = report.nodes_by_kind.iter().collect();
        node_kinds.sort_by(|a, b| b.1.cmp(a.1));
        for (kind, count) in node_kinds.into_iter().take(10) {
            println!("  {kind}: {count}");
        }
    }
    if !report.edges_by_kind.is_empty() {
        println!("Edges by kind (top 10):");
        let mut edge_kinds: Vec<_> = report.edges_by_kind.iter().collect();
        edge_kinds.sort_by(|a, b| b.1.cmp(a.1));
        for (kind, count) in edge_kinds.into_iter().take(10) {
            println!("  {kind}: {count}");
        }
    }
}

// =============================================================================
// evidence (#kgadactivate — agent-doc's KG read seam per spec line 29-30)
// =============================================================================

pub(crate) fn cmd_kg_evidence(
    symbol: Option<String>,
    kind: Option<String>,
    limit: usize,
    graph_db: Option<PathBuf>,
    json: bool,
) -> Result<()> {
    let db_path = graph_db
        .unwrap_or_else(|| PathBuf::from(DEFAULT_GRAPH_DB_RELATIVE));
    let mut query = tsift_agent_doc::graph_evidence::GraphEvidenceQuery::default().with_limit(limit);
    if let Some(symbol) = symbol {
        query = query.with_symbol(symbol);
    }
    if let Some(kind) = kind {
        query = query.with_kind(kind);
    }
    let report =
        tsift_agent_doc::graph_evidence::read_graph_evidence_from_db(&db_path, &query)?;

    if json {
        println!("{}", serde_json::to_string_pretty(&report)?);
        return Ok(());
    }

    println!("KG evidence");
    println!("Graph DB: {}", report.graph_db);
    if !report.exists {
        println!("  (no graph.db at this path — run `tsift kg extract --graph-db <path>` to populate)");
        return Ok(());
    }
    println!(
        "Total: {} nodes / {} edges.  Matched: {} (limit {})",
        report.total_nodes_in_db,
        report.total_edges_in_db,
        report.matched_nodes.len(),
        report.query.limit,
    );
    if let Some(symbol) = &report.query.symbol {
        println!("Symbol filter: {symbol}");
    }
    if let Some(kind) = &report.query.kind {
        println!("Kind filter: {kind}");
    }
    if report.matched_nodes.is_empty() {
        println!("(no matches)");
        return Ok(());
    }
    for node in &report.matched_nodes {
        let sources = if node.provenance_systems.is_empty() {
            String::new()
        } else {
            format!(" [{}]", node.provenance_systems.join(", "))
        };
        let refs = if node.source_refs.is_empty() {
            String::new()
        } else {
            format!(" — {}", node.source_refs.join(", "))
        };
        println!(
            "  - {} [{}]{} ({} edges){}",
            node.label, node.kind, sources, node.incident_edge_count, refs,
        );
    }
    Ok(())
}

// =============================================================================
// unload (#kgunloadpost — build_unload_actions owns execution, called from CLI)
// =============================================================================

#[derive(Debug, Serialize)]
struct KgUnloadReport {
    profile_id: String,
    model_tag: String,
    endpoint: String,
    actions: Vec<tsift_local_model::UnloadActionResult>,
}

pub(crate) fn cmd_kg_unload(
    profile: Option<String>,
    model: Option<String>,
    host: Option<String>,
    json: bool,
) -> Result<()> {
    let profile_id = profile.unwrap_or_else(|| DEFAULT_PROFILE_ID.to_string());
    let profile_handle = profile_by_id(&profile_id).ok_or_else(|| {
        anyhow::anyhow!(
            "unknown tsift local-model profile `{profile_id}`. Use --model <ollama-tag> or one of: {}",
            list_profile_ids().join(", ")
        )
    })?;
    let model_tag = model.unwrap_or_else(|| profile_handle.model_ref.to_string());
    let endpoint = resolve_ollama_host_for_unload(host.as_deref());

    // The planner owns execution (#kgunloadpost): build_unload_actions still
    // plans, and execute_unload_actions POSTs the ollama keep_alive:0 with
    // the resolved model tag (so --model override actually takes effect).
    let actions = tsift_local_model::build_unload_actions(
        &profile_handle,
        Some(endpoint.as_str()),
        None,
    );
    let results = tsift_local_model::execute_unload_actions(&actions, &model_tag);

    let report = KgUnloadReport {
        profile_id,
        model_tag,
        endpoint,
        actions: results,
    };

    if json {
        println!("{}", serde_json::to_string_pretty(&report)?);
    } else {
        println!("KG unload");
        println!("Profile: {} ({})", report.profile_id, report.model_tag);
        println!("Endpoint: {}", report.endpoint);
        for action in &report.actions {
            println!(
                "  - {}: {} ({})",
                action.label,
                if action.executed { "executed" } else { "skipped" },
                action.outcome
            );
        }
    }
    Ok(())
}

fn resolve_ollama_host_for_unload(host_override: Option<&str>) -> String {
    if let Some(host) = host_override
        && !host.trim().is_empty()
    {
        return host.trim_end_matches('/').to_string();
    }
    tsift_local_model::resolve_provider_endpoint(
        &tsift_local_model::UnloadStrategy::OllamaKeepAliveZero,
        None,
    )
    .trim_end_matches('/')
    .to_string()
}

// =============================================================================
// smoke (spec local-kg-model.md line 31-32)
// =============================================================================

#[derive(Debug, Serialize)]
struct KgSmokeReport {
    profile_id: Option<String>,
    model_tag: String,
    host: String,
    chunks: usize,
    entities: usize,
    relations: usize,
    unloaded: bool,
}

pub(crate) fn cmd_kg_smoke(
    profile: Option<String>,
    model: Option<String>,
    host: Option<String>,
    unload_after: bool,
    json: bool,
) -> Result<()> {
    let resolved_model = resolve_model_tag(profile.as_deref(), model.as_deref())?;
    let extractor = match host.as_deref() {
        Some(host) => OllamaKgExtractor::new(&resolved_model).with_host(host),
        None => OllamaKgExtractor::new(&resolved_model),
    };
    let document =
        KgInputDocument::new(KgInputKind::Source, SMOKE_SOURCE_REF, SMOKE_SAMPLE_TEXT);
    let report = extract_documents_to_projection(&[document], &extractor, ChunkingConfig::default())
        .context("KG smoke extraction failed — is the Ollama server running and the model pulled?")?;
    let entities = report.projection.nodes.len();
    let relations = report.projection.edges.len();

    let mut unloaded = false;
    if unload_after {
        let outcome = tsift_local_model::unload_model_at(extractor.host(), &resolved_model);
        if outcome.executed {
            unloaded = true;
        } else {
            eprintln!("kg smoke: unload after run failed: {}", outcome.outcome);
        }
    }

    let summary = KgSmokeReport {
        profile_id: profile.clone(),
        model_tag: extractor.model().to_string(),
        host: extractor.host().to_string(),
        chunks: report.chunks.len(),
        entities,
        relations,
        unloaded,
    };

    if json {
        println!("{}", serde_json::to_string_pretty(&summary)?);
    } else {
        println!("KG smoke");
        println!(
            "Model: {} ({})",
            summary.model_tag, summary.host
        );
        println!(
            "Chunks: {}   Entities: {}   Relations: {}",
            summary.chunks, summary.entities, summary.relations
        );
        if summary.unloaded {
            println!("Unloaded: yes");
        }
        println!("OK");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;
    use tsift_core::{GraphEdge, GraphNode, GraphProjection};
    use tsift_sqlite::SqliteGraphStore;

    #[test]
    fn kg_status_reports_missing_db_cleanly() {
        let dir = TempDir::new().unwrap();
        let missing = dir.path().join("does-not-exist.db");
        let report = build_kg_status_report(&missing).expect("missing db should not error");
        assert!(!report.exists);
        assert_eq!(report.total_nodes, 0);
        assert_eq!(report.total_edges, 0);
        assert!(report.nodes_by_kind.is_empty());
        assert!(report.edges_by_kind.is_empty());
        assert!(report.graph_db.contains("does-not-exist.db"));
    }

    #[test]
    fn kg_status_counts_nodes_and_edges_by_kind() {
        let dir = TempDir::new().unwrap();
        let db_path = dir.path().join("graph.db");
        let mut store = SqliteGraphStore::open(&db_path).expect("open writable store");

        let mut projection = GraphProjection::default();
        projection.nodes.push(
            GraphNode::new("n:kg-1", "kg_source", "tsift-kg")
                .with_property("provider", "tsift-kg"),
        );
        projection.nodes.push(
            GraphNode::new("n:kg-2", "kg_source", "OllamaKgExtractor")
                .with_property("provider", "tsift-kg"),
        );
        projection.nodes.push(GraphNode::new("n:other", "concept", "lease"));
        projection.edges.push(GraphEdge::new("n:kg-1", "n:kg-2", "calls"));
        projection.edges.push(GraphEdge::new("n:kg-1", "n:other", "related_to"));
        store
            .upsert_projection(&projection)
            .expect("upsert projection");

        let report = build_kg_status_report(&db_path).expect("populated db status");
        assert!(report.exists);
        assert_eq!(report.total_nodes, 3);
        assert_eq!(report.total_edges, 2);
        assert_eq!(report.nodes_by_kind.get("kg_source"), Some(&2));
        assert_eq!(report.nodes_by_kind.get("concept"), Some(&1));
        assert_eq!(report.edges_by_kind.get("calls"), Some(&1));
        assert_eq!(report.edges_by_kind.get("related_to"), Some(&1));
    }

    #[test]
    fn resolve_ollama_host_for_unload_uses_explicit_override() {
        let host = resolve_ollama_host_for_unload(Some("http://192.168.1.17:11434"));
        assert_eq!(host, "http://192.168.1.17:11434");
    }

    #[test]
    fn resolve_ollama_host_for_unload_falls_back_when_blank() {
        let host = resolve_ollama_host_for_unload(Some("   "));
        // Resolves to the default ollama endpoint exposed by tsift-local-model.
        assert!(!host.is_empty());
        assert!(host.starts_with("http"));
    }
}
