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

/// Arguments for `tsift kg extract` (#kgleasewire adds lease coordination).
pub(crate) struct KgExtractArgs {
    pub profile: Option<String>,
    pub model: Option<String>,
    pub host: Option<String>,
    pub input: Option<PathBuf>,
    pub source_ref: Option<String>,
    pub graph_db: Option<PathBuf>,
    pub no_lease: bool,
    pub idle_ttl_seconds: u64,
    pub keep_loaded: bool,
    pub lease_file: Option<PathBuf>,
    pub json: bool,
}

/// Resolve the profile id used for cooperative GPU leasing during an extract.
///
/// An explicit `--model` bypasses profile resolution entirely, so there is no
/// profile to lease (returns `None`). Otherwise the chosen profile — or the
/// default — is leased so concurrent extracts serialize on the GPU.
pub(crate) fn resolve_lease_profile_id(
    profile: Option<&str>,
    model: Option<&str>,
) -> Option<String> {
    if model.is_some() {
        return None;
    }
    Some(profile.unwrap_or(DEFAULT_PROFILE_ID).to_string())
}

pub(crate) fn cmd_kg_extract(args: KgExtractArgs) -> Result<()> {
    let KgExtractArgs {
        profile,
        model,
        host,
        input,
        source_ref,
        graph_db,
        no_lease,
        idle_ttl_seconds,
        keep_loaded,
        lease_file,
        json,
    } = args;
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

    // #kgleasewire: acquire an exclusive cooperative GPU lease before loading
    // the model so concurrent extracts serialize on one GPU. A bailed extract
    // leaves a pid-dead holder that `kg`/`lease reap` reclaims (crash-safe via
    // #kgreflease), so we only release on the success path.
    let lease_profile = if no_lease {
        None
    } else {
        resolve_lease_profile_id(profile.as_deref(), model.as_deref())
    };
    let lease_path = tsift_local_model::resolve_lease_file(lease_file.as_deref());
    let holder_pid = std::process::id();
    if let Some(ref lease_id) = lease_profile {
        let acquisition = tsift_local_model::acquire_lease(
            lease_id,
            holder_pid,
            "tsift kg extract",
            0,
            idle_ttl_seconds,
            tsift_local_model::current_unix_seconds(),
            &lease_path,
        )
        .with_context(|| format!("acquiring GPU lease for {lease_id}"))?;
        if acquisition.status == tsift_local_model::GpuLeaseAcquisitionStatus::Conflict {
            let held_by = acquisition
                .conflict
                .map(|c| c.holder_pid.to_string())
                .unwrap_or_else(|| "unknown".to_string());
            bail!(
                "GPU lease for {lease_id} is held by pid {held_by}; another extractor is \
                 running. Wait for it, or pass --no-lease to bypass coordination."
            );
        }
    }

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

    // #kgleasewire: release the lease; reference-counted unload when this
    // extract dropped the last live reference (unless --keep-loaded).
    if let Some(ref lease_id) = lease_profile {
        let release = tsift_local_model::release_lease(
            lease_id,
            holder_pid,
            tsift_local_model::current_unix_seconds(),
            &lease_path,
        )
        .with_context(|| format!("releasing GPU lease for {lease_id}"))?;
        if !keep_loaded && release.remaining_holders == 0 {
            let endpoint = resolve_ollama_host_for_unload(host.as_deref());
            let outcome = tsift_local_model::unload_model_at(&endpoint, &resolved_model);
            if !json {
                println!(
                    "Unloaded {} (last lease reference released): {}",
                    resolved_model, outcome.outcome
                );
            }
        }
    }

    Ok(())
}

// =============================================================================
// refresh (#kgextractrefresh — on-demand staleness detection)
// =============================================================================

/// Staleness of one recorded `kg_source` against the current working tree.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum RefreshStatus {
    /// File content differs from the hash recorded at extraction.
    Stale,
    /// File content matches the recorded hash.
    Unchanged,
    /// `source_ref` is not a readable file (deleted, moved, or a non-path label).
    Missing,
    /// Extracted before `#kgextractrefresh` recorded content hashes; staleness
    /// is unknown, so a refresh is recommended.
    NoRecordedHash,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(crate) struct RefreshEntry {
    pub source_ref: String,
    pub status: RefreshStatus,
    pub recorded_hash: Option<String>,
    pub current_hash: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(crate) struct RefreshPlan {
    pub entries: Vec<RefreshEntry>,
}

impl RefreshPlan {
    /// Entries that warrant re-extraction (changed or unknown-staleness).
    pub fn needs_refresh(&self) -> Vec<&RefreshEntry> {
        self.entries
            .iter()
            .filter(|e| matches!(e.status, RefreshStatus::Stale | RefreshStatus::NoRecordedHash))
            .collect()
    }
}

/// Pure staleness planner: classify each recorded `(source_ref, recorded_hash)`
/// against the current content hash returned by `current_hash` (`None` when the
/// source is not a readable file).
pub(crate) fn plan_refresh(
    recorded: &[(String, Option<String>)],
    current_hash: impl Fn(&str) -> Option<String>,
) -> RefreshPlan {
    let mut entries = Vec::with_capacity(recorded.len());
    for (source_ref, recorded_hash) in recorded {
        let current = current_hash(source_ref);
        let status = match (recorded_hash, &current) {
            (None, _) => RefreshStatus::NoRecordedHash,
            (Some(_), None) => RefreshStatus::Missing,
            (Some(r), Some(c)) if r == c => RefreshStatus::Unchanged,
            (Some(_), Some(_)) => RefreshStatus::Stale,
        };
        entries.push(RefreshEntry {
            source_ref: source_ref.clone(),
            status,
            recorded_hash: recorded_hash.clone(),
            current_hash: current,
        });
    }
    RefreshPlan { entries }
}

/// Blake3 hash of a file's contents, or `None` when it is not a readable file
/// (matches the recorded `source_content_hash` written at extraction).
fn file_content_hash(source_ref: &str) -> Option<String> {
    let path = Path::new(source_ref);
    if !path.is_file() {
        return None;
    }
    std::fs::read(path)
        .ok()
        .map(|bytes| blake3::hash(&bytes).to_hex().to_string())
}

pub(crate) fn cmd_kg_refresh(graph_db: Option<PathBuf>, json: bool) -> Result<()> {
    let db_path = graph_db.unwrap_or_else(|| PathBuf::from(DEFAULT_GRAPH_DB_RELATIVE));
    if !db_path.exists() {
        if json {
            println!(
                "{}",
                serde_json::to_string_pretty(&serde_json::json!({
                    "graph_db": db_path.display().to_string(),
                    "exists": false,
                    "entries": [],
                }))?
            );
        } else {
            println!(
                "no graph.db at {} — run `tsift kg extract --graph-db <path>` first",
                db_path.display()
            );
        }
        return Ok(());
    }

    let store = tsift_sqlite::SqliteGraphStore::open_read_only_resilient(&db_path)
        .with_context(|| format!("opening graph.db read-only at {}", db_path.display()))?;
    let nodes = store
        .all_nodes()
        .with_context(|| "reading graph_nodes for kg refresh")?;
    let recorded: Vec<(String, Option<String>)> = nodes
        .iter()
        .filter(|n| n.kind == "kg_source")
        .map(|n| {
            let source_ref = n
                .properties
                .get("source_ref")
                .cloned()
                .unwrap_or_else(|| n.label.clone());
            let recorded_hash = n.properties.get("source_content_hash").cloned();
            (source_ref, recorded_hash)
        })
        .collect();

    let plan = plan_refresh(&recorded, file_content_hash);
    let needs = plan.needs_refresh();

    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({
                "graph_db": db_path.display().to_string(),
                "exists": true,
                "sources": plan.entries.len(),
                "needs_refresh": needs.len(),
                "entries": plan.entries,
            }))?
        );
    } else {
        println!("KG refresh plan ({})", db_path.display());
        println!(
            "Sources: {}  Needs refresh: {}",
            plan.entries.len(),
            needs.len()
        );
        for entry in &plan.entries {
            println!("  - {} [{:?}]", entry.source_ref, entry.status);
        }
        if !needs.is_empty() {
            println!("\nRe-extract stale/unknown sources with:");
            for entry in &needs {
                println!(
                    "  tsift kg extract --input {0} --source-ref {0} --graph-db {1}",
                    entry.source_ref,
                    db_path.display()
                );
            }
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
    fn plan_refresh_classifies_each_source() {
        let recorded = vec![
            ("a.md".to_string(), Some("hash-a".to_string())), // unchanged
            ("b.md".to_string(), Some("hash-b-old".to_string())), // stale
            ("c.md".to_string(), Some("hash-c".to_string())), // missing (no current)
            ("d.md".to_string(), None),                       // no recorded hash
        ];
        let current = |s: &str| match s {
            "a.md" => Some("hash-a".to_string()),
            "b.md" => Some("hash-b-new".to_string()),
            "c.md" => None,
            "d.md" => Some("hash-d".to_string()),
            _ => None,
        };
        let plan = plan_refresh(&recorded, current);
        assert_eq!(plan.entries[0].status, RefreshStatus::Unchanged);
        assert_eq!(plan.entries[1].status, RefreshStatus::Stale);
        assert_eq!(plan.entries[2].status, RefreshStatus::Missing);
        assert_eq!(plan.entries[3].status, RefreshStatus::NoRecordedHash);
        // Stale + NoRecordedHash warrant re-extraction; Unchanged + Missing do not.
        let needs: Vec<_> = plan.needs_refresh().iter().map(|e| e.source_ref.clone()).collect();
        assert_eq!(needs, vec!["b.md".to_string(), "d.md".to_string()]);
    }

    #[test]
    fn plan_refresh_empty_when_no_sources() {
        let plan = plan_refresh(&[], |_| None);
        assert!(plan.entries.is_empty());
        assert!(plan.needs_refresh().is_empty());
    }

    #[test]
    fn resolve_lease_profile_id_defaults_when_no_profile_or_model() {
        assert_eq!(
            resolve_lease_profile_id(None, None).as_deref(),
            Some(DEFAULT_PROFILE_ID)
        );
    }

    #[test]
    fn resolve_lease_profile_id_uses_explicit_profile() {
        assert_eq!(
            resolve_lease_profile_id(Some("qwen3-32b-q4"), None).as_deref(),
            Some("qwen3-32b-q4")
        );
    }

    #[test]
    fn resolve_lease_profile_id_none_when_explicit_model_bypasses_profiles() {
        // An explicit --model bypasses profile resolution, so there is no
        // profile to lease — extract proceeds without GPU lease coordination.
        assert_eq!(resolve_lease_profile_id(None, Some("some-ollama-tag")), None);
        assert_eq!(
            resolve_lease_profile_id(Some("qwen3-32b-q4"), Some("some-ollama-tag")),
            None
        );
    }

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
