use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;
use tsift_core::{GraphEdge, GraphFreshness, GraphNode, GraphProjection, GraphProvenance};
use tsift_memory::{
    DEFAULT_MEMORY_CANDIDATE_LIMIT, MemoryEvent, MemoryReadPolicy, MemoryReadWatermark,
    estimate_tokens, memory_read_watermark, read_memory_event_candidates, read_memory_events,
    read_memory_events_with_policy,
};
use tsift_sqlite::SqliteGraphStore;

pub const MEMGRAPHRAG_CONTRACT_VERSION: &str = "tsift-memgraphrag-v1";
pub const SEMANTIC_EMBEDDING_MODEL: &str = "tsift-local-hash-v1";

const SEMANTIC_EMBEDDING_DIM: usize = 32;
const DEFAULT_TRAVERSAL_MEMORY_EVENT_LIMIT: usize = 600;
const MEMORY_RANK_CANDIDATE_MULTIPLIER: usize = 8;
const MEMORY_PROJECTION_NODE_ID: &str = "memory_projection:tsift-memory";

pub fn memory_graph_node_kinds() -> Vec<&'static str> {
    vec![
        "memory_session",
        "memory_event",
        "session",
        "source_handle",
        "semantic_concept",
        "semantic_vector_handle",
        "memory_projection",
    ]
}

pub fn project_memory_events(events: &[MemoryEvent]) -> GraphProjection {
    let mut projection = GraphProjection::default();
    let mut sessions = BTreeSet::new();

    for event in events {
        let event_id = event.stable_id();
        if let Some(session_id) = &event.session_id
            && sessions.insert(session_id.clone())
        {
            projection.nodes.push(
                GraphNode::new(
                    format!("memsess:{}", blake3::hash(session_id.as_bytes()).to_hex()),
                    "memory_session",
                    session_id,
                )
                .with_property("session_id", session_id)
                .with_provenance(GraphProvenance::new("tsift-memory", session_id)),
            );
        }

        let mut node = GraphNode::new(&event_id, "memory_event", event.kind.as_str())
            .with_property("event_kind", event.kind.as_str())
            .with_property("source_ref", &event.source_ref)
            .with_property("token_estimate", event.token_estimate.to_string())
            .with_provenance(GraphProvenance::new("tsift-memory", &event.source_ref));
        if let Some(imported_from) = &event.imported_from {
            node = node.with_property("imported_from", imported_from);
        }
        if let Some(imported_id) = &event.imported_id {
            node = node.with_property("imported_id", imported_id);
        }
        projection.nodes.push(node);

        if let Some(session_id) = &event.session_id {
            let session_node_id =
                format!("memsess:{}", blake3::hash(session_id.as_bytes()).to_hex());
            projection.edges.push(
                GraphEdge::new(session_node_id, event_id, "records_memory_event")
                    .with_provenance(GraphProvenance::new("tsift-memory", &event.source_ref)),
            );
        }
    }

    projection
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct MemoryDecayConfig {
    pub half_life_secs: f64,
    pub lexical_weight: f64,
    pub recency_weight: f64,
}

impl Default for MemoryDecayConfig {
    fn default() -> Self {
        Self {
            half_life_secs: 7.0 * 24.0 * 3600.0,
            lexical_weight: 0.6,
            recency_weight: 0.4,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ScoredMemoryEvent {
    pub event: MemoryEvent,
    pub lexical_score: f64,
    pub recency_score: f64,
    pub score: f64,
}

fn memory_query_terms(query: &str) -> Vec<String> {
    query
        .split(|c: char| !c.is_alphanumeric())
        .filter(|term| !term.is_empty())
        .map(|term| term.to_lowercase())
        .collect()
}

fn memory_lexical_overlap(terms: &[String], text: &str) -> f64 {
    if terms.is_empty() {
        return 0.0;
    }
    let haystack = text.to_lowercase();
    let hits = terms
        .iter()
        .filter(|term| haystack.contains(term.as_str()))
        .count();
    hits as f64 / terms.len() as f64
}

fn memory_recency_decay(observed_at_unix: Option<i64>, now_unix: i64, half_life_secs: f64) -> f64 {
    match observed_at_unix {
        Some(observed) => {
            let age = (now_unix - observed).max(0) as f64;
            0.5f64.powf(age / half_life_secs.max(1.0))
        }
        None => 0.0,
    }
}

pub fn rank_memory_events(
    events: &[MemoryEvent],
    query: &str,
    now_unix: i64,
    config: MemoryDecayConfig,
    limit: usize,
) -> Vec<ScoredMemoryEvent> {
    let terms = memory_query_terms(query);
    let mut scored: Vec<ScoredMemoryEvent> = events
        .iter()
        .map(|event| {
            let lexical_score = memory_lexical_overlap(&terms, &event.text);
            let recency_score =
                memory_recency_decay(event.observed_at_unix, now_unix, config.half_life_secs);
            let score =
                config.lexical_weight * lexical_score + config.recency_weight * recency_score;
            ScoredMemoryEvent {
                event: event.clone(),
                lexical_score,
                recency_score,
                score,
            }
        })
        .collect();
    scored.sort_by(|a, b| {
        b.score
            .partial_cmp(&a.score)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| {
                b.recency_score
                    .partial_cmp(&a.recency_score)
                    .unwrap_or(std::cmp::Ordering::Equal)
            })
    });
    scored.truncate(limit);
    scored
}

pub fn memory_rank_candidate_limit(limit: usize) -> usize {
    if limit == 0 {
        return 0;
    }
    limit
        .saturating_mul(MEMORY_RANK_CANDIDATE_MULTIPLIER)
        .min(DEFAULT_MEMORY_CANDIDATE_LIMIT.max(limit))
}

pub fn rank_memory_event_candidates(
    memory_db: &Path,
    query: &str,
    now_unix: i64,
    config: MemoryDecayConfig,
    limit: usize,
) -> Result<Vec<ScoredMemoryEvent>> {
    let candidate_limit = memory_rank_candidate_limit(limit);
    let candidates = read_memory_event_candidates(memory_db, query, candidate_limit)?;
    Ok(rank_memory_events(
        &candidates,
        query,
        now_unix,
        config,
        limit,
    ))
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MemoryQueryPlan {
    pub contract_version: String,
    pub query: String,
    pub limit: usize,
    pub candidate_limit: usize,
    pub max_tokens: usize,
    pub estimated_query_tokens: usize,
    pub decay: MemoryDecayConfig,
    pub output_contract: Vec<String>,
    pub next_commands: Vec<String>,
}

pub fn plan_memory_query(query: &str, limit: usize, max_tokens: usize) -> Result<MemoryQueryPlan> {
    if query.trim().is_empty() {
        bail!("memory query must not be empty");
    }
    Ok(MemoryQueryPlan {
        contract_version: MEMGRAPHRAG_CONTRACT_VERSION.to_string(),
        query: query.to_string(),
        limit,
        candidate_limit: memory_rank_candidate_limit(limit),
        max_tokens,
        estimated_query_tokens: estimate_tokens(query),
        decay: MemoryDecayConfig::default(),
        output_contract: vec![
            "indexed FTS/recent candidate set capped before ranking".to_string(),
            "decay-weighted ranked memory_event ids (lexical + recency)".to_string(),
            "per-event lexical_score, recency_score, and blended score".to_string(),
            "source_ref handles for expansion".to_string(),
            "graph node ids for neighborhood projection".to_string(),
            "token estimates for every returned packet".to_string(),
        ],
        next_commands: vec![
            "tsift memory status . --json".to_string(),
            "tsift memory project-graph . --json".to_string(),
            "tsift graph-db --path . --json related '<query>'".to_string(),
        ],
    })
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MemoryGraphProjectReport {
    pub events_projected: usize,
    pub nodes_upserted: usize,
    pub edges_upserted: usize,
    pub read_policy: MemoryReadPolicy,
    pub source_watermark: String,
    pub content_hash: String,
    pub events_available: usize,
}

pub fn project_memory_into_graph(
    memory_db: &Path,
    graph_db: &Path,
    limit: usize,
) -> Result<MemoryGraphProjectReport> {
    project_memory_into_graph_with_policy(memory_db, graph_db, limit, &MemoryReadPolicy::default())
}

pub fn project_memory_into_graph_with_policy(
    memory_db: &Path,
    graph_db: &Path,
    limit: usize,
    read_policy: &MemoryReadPolicy,
) -> Result<MemoryGraphProjectReport> {
    let events = read_memory_events_with_policy(memory_db, read_policy, limit)?;
    let watermark = memory_read_watermark(memory_db, read_policy, limit, &events)?;
    let mut projection = project_memory_events(&events);
    append_memory_projection_metadata(&mut projection, &watermark)?;
    let nodes_upserted = projection.nodes.len();
    let edges_upserted = projection.edges.len();
    if let Some(parent) = graph_db.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("create graph db dir {}", parent.display()))?;
    }
    let mut store = SqliteGraphStore::open(graph_db)
        .with_context(|| format!("open graph store {}", graph_db.display()))?;
    store.upsert_projection(&projection)?;
    Ok(MemoryGraphProjectReport {
        events_projected: events.len(),
        nodes_upserted,
        edges_upserted,
        read_policy: read_policy.clone(),
        source_watermark: watermark.source_watermark,
        content_hash: watermark.content_hash,
        events_available: watermark.events_available,
    })
}

fn append_memory_projection_metadata(
    projection: &mut GraphProjection,
    watermark: &MemoryReadWatermark,
) -> Result<()> {
    let mut node = GraphNode::new(
        MEMORY_PROJECTION_NODE_ID,
        "memory_projection",
        "tsift-memory graph projection",
    )
    .with_property("handle", MEMORY_PROJECTION_NODE_ID)
    .with_property("ref_id", "tsift-memory")
    .with_property("provider", "tsift-memory")
    .with_property("read_policy", watermark.policy.order.as_str())
    .with_property("limit", watermark.limit.to_string())
    .with_property("events_read", watermark.events_read.to_string())
    .with_property("events_available", watermark.events_available.to_string())
    .with_property("source_watermark", watermark.source_watermark.clone())
    .with_property("content_hash", watermark.content_hash.clone())
    .with_provenance(
        GraphProvenance::new("tsift-memory", "memory_events")
            .with_content_hash(watermark.content_hash.clone()),
    )
    .with_freshness(GraphFreshness::content_hash(
        watermark.source_watermark.clone(),
    ));
    if let Some(query) = &watermark.policy.query {
        node = node.with_property("query", query.clone());
    }
    if let Some(max_rowid) = watermark.max_rowid {
        node = node.with_property("max_rowid", max_rowid.to_string());
    }
    if let Some(max_observed_at_unix) = watermark.max_observed_at_unix {
        node = node.with_property("max_observed_at_unix", max_observed_at_unix.to_string());
    }
    if let Some(max_created_at_unix) = watermark.max_created_at_unix {
        node = node.with_property("max_created_at_unix", max_created_at_unix.to_string());
    }
    projection.nodes.push(node_with_content_freshness(node)?);
    Ok(())
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MemoryOntologyGraphReport {
    pub type_nodes: usize,
    pub relations: usize,
}

pub fn derive_memory_ontology_graph(graph_db: &Path) -> Result<MemoryOntologyGraphReport> {
    if !graph_db.exists() {
        bail!(
            "graph store {} does not exist; run `tsift graph-db refresh` or `tsift memory project-graph` first",
            graph_db.display()
        );
    }
    let mut store = SqliteGraphStore::open(graph_db)
        .with_context(|| format!("open graph store {}", graph_db.display()))?;
    let ontology = store.derive_ontology()?;
    let type_nodes = ontology.nodes.len();
    let relations = ontology.edges.len();
    store.upsert_projection(&ontology)?;
    Ok(MemoryOntologyGraphReport {
        type_nodes,
        relations,
    })
}

pub fn append_tsift_memory_graph_projection_rows(
    root: &Path,
    nodes: &mut Vec<GraphNode>,
    edges: &mut Vec<GraphEdge>,
) -> Result<()> {
    append_tsift_memory_graph_projection_rows_with_limit(
        root,
        nodes,
        edges,
        DEFAULT_TRAVERSAL_MEMORY_EVENT_LIMIT,
    )
}

pub fn append_tsift_memory_graph_projection_rows_with_limit(
    root: &Path,
    nodes: &mut Vec<GraphNode>,
    edges: &mut Vec<GraphEdge>,
    event_limit: usize,
) -> Result<()> {
    let memory_db = tsift_memory::default_memory_db_path(root);
    if !memory_db.exists() {
        return Ok(());
    }
    let events = match read_memory_events(&memory_db, event_limit) {
        Ok(events) => events,
        Err(_) => return Ok(()),
    };
    append_memory_events_as_traversal_rows(root, &events, nodes, edges)
}

pub fn append_memory_events_as_traversal_rows(
    root: &Path,
    events: &[MemoryEvent],
    nodes: &mut Vec<GraphNode>,
    edges: &mut Vec<GraphEdge>,
) -> Result<()> {
    if events.is_empty() {
        return Ok(());
    }

    let mut seen_sessions = BTreeSet::new();
    let mut edge_map = BTreeMap::<(String, String, String), GraphEdge>::new();

    for event in events {
        let event_id = event.stable_id();
        let event_key = memory_event_key(event);
        let source_handle = stable_handle("tmemsrc", &event_key);
        let semantic_handle = stable_handle("tmemsem", &event_key);
        let provenance = GraphProvenance::new("tsift-memory", &event.source_ref);
        let imported_from = event.imported_from.as_deref().unwrap_or("native");

        if let Some(session_id) = &event.session_id {
            let session_handle =
                format!("memsess:{}", blake3::hash(session_id.as_bytes()).to_hex());
            if seen_sessions.insert(session_id.clone()) {
                let session_node = GraphNode::new(
                    session_handle.clone(),
                    "memory_session",
                    truncate_for_compact(session_id, 80),
                )
                .with_property("handle", session_handle.clone())
                .with_property("ref_id", session_id.clone())
                .with_property("session_id", session_id.clone())
                .with_property("provider", "tsift-memory")
                .with_property(
                    "expand",
                    format!(
                        "tsift memory status {} --json",
                        shell_quote(root.to_string_lossy().as_ref())
                    ),
                )
                .with_provenance(provenance.clone());
                nodes.push(node_with_content_freshness(session_node)?);
            }

            insert_semantic_edge(
                &mut edge_map,
                GraphEdge::new(
                    session_handle.clone(),
                    event_id.clone(),
                    "records_memory_event",
                )
                .with_property("label", "tsift-memory session event")
                .with_provenance(provenance.clone()),
            );
            insert_semantic_edge(
                &mut edge_map,
                GraphEdge::new(
                    session_handle,
                    source_handle.clone(),
                    "records_memory_source",
                )
                .with_property("label", "tsift-memory session source")
                .with_provenance(provenance.clone()),
            );
        }

        let label = memory_event_label(event);
        let mut event_node = GraphNode::new(event_id.clone(), "memory_event", event.kind.as_str())
            .with_property("handle", event_id.clone())
            .with_property("ref_id", event.source_ref.clone())
            .with_property("source_ref", event.source_ref.clone())
            .with_property("provider", "tsift-memory")
            .with_property("memory_kind", event.kind.as_str())
            .with_property("imported_from", imported_from)
            .with_property("text_preview", truncate_for_compact(&event.text, 240))
            .with_property("token_estimate", event.token_estimate.to_string())
            .with_property(
                "expand",
                format!(
                    "tsift memory status {} --json",
                    shell_quote(root.to_string_lossy().as_ref())
                ),
            )
            .with_provenance(provenance.clone());
        if let Some(session_id) = &event.session_id {
            event_node = event_node.with_property("session_id", session_id.clone());
        }
        if let Some(observed_at_unix) = event.observed_at_unix {
            event_node = event_node.with_property("observed_at_unix", observed_at_unix.to_string());
        }
        if let Some(imported_id) = &event.imported_id {
            event_node = event_node.with_property("imported_id", imported_id.clone());
        }
        nodes.push(node_with_content_freshness(event_node)?);

        let mut source_node = GraphNode::new(source_handle.clone(), "source_handle", label.clone())
            .with_property("handle", source_handle.clone())
            .with_property("ref_id", event.source_ref.clone())
            .with_property("source_ref", event.source_ref.clone())
            .with_property("provider", "tsift-memory")
            .with_property("memory_kind", event.kind.as_str())
            .with_property("imported_from", imported_from)
            .with_property("text_preview", truncate_for_compact(&event.text, 240))
            .with_property("token_estimate", event.token_estimate.to_string())
            .with_property(
                "expand",
                format!(
                    "tsift memory status {} --json",
                    shell_quote(root.to_string_lossy().as_ref())
                ),
            )
            .with_provenance(provenance.clone());
        if let Some(session_id) = &event.session_id {
            source_node = source_node.with_property("session_id", session_id.clone());
        }
        if let Some(observed_at_unix) = event.observed_at_unix {
            source_node =
                source_node.with_property("observed_at_unix", observed_at_unix.to_string());
        }
        if let Some(imported_id) = &event.imported_id {
            source_node = source_node.with_property("imported_id", imported_id.clone());
        }
        nodes.push(node_with_content_freshness(source_node)?);

        insert_semantic_edge(
            &mut edge_map,
            GraphEdge::new(event_id.clone(), source_handle.clone(), "projects_source")
                .with_property("label", "tsift-memory source projection")
                .with_provenance(provenance.clone()),
        );

        let semantic_text = format!("{} {}", label, event.text);
        let semantic_node =
            GraphNode::new(semantic_handle.clone(), "semantic_concept", label.clone())
                .with_property("handle", semantic_handle.clone())
                .with_property("ref_id", event.source_ref.clone())
                .with_property("detail", "semantic row from tsift-memory")
                .with_property("source_ref", event.source_ref.clone())
                .with_property("provider", "tsift-memory")
                .with_property("memory_kind", event.kind.as_str())
                .with_property("imported_from", imported_from)
                .with_property("embedding_model", SEMANTIC_EMBEDDING_MODEL)
                .with_property("embedding", semantic_embedding_property(&semantic_text))
                .with_property(
                    "expand",
                    semantic_related_command(root, &label, SemanticRelatedKind::Concept),
                )
                .with_provenance(provenance.clone());
        nodes.push(node_with_content_freshness(semantic_node)?);

        insert_semantic_edge(
            &mut edge_map,
            GraphEdge::new(
                source_handle.clone(),
                semantic_handle.clone(),
                "mentions_concept",
            )
            .with_property("label", "tsift-memory semantic source")
            .with_provenance(provenance.clone()),
        );
    }

    for edge in edge_map.into_values() {
        edges.push(edge_with_content_freshness(edge)?);
    }

    Ok(())
}

fn memory_event_key(event: &MemoryEvent) -> String {
    match (event.imported_from.as_deref(), event.imported_id.as_deref()) {
        (Some(imported_from), Some(imported_id)) => {
            format!("{imported_from}:{imported_id}")
        }
        _ => event.stable_id(),
    }
}

fn memory_event_label(event: &MemoryEvent) -> String {
    let first_line = event
        .text
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .unwrap_or(event.kind.as_str());
    match event.kind.as_str() {
        "imported_observation" => {
            let observation_type = event
                .metadata
                .get("observation_type")
                .map(String::as_str)
                .unwrap_or("observation");
            truncate_for_compact(&format!("{observation_type}: {first_line}"), 80)
        }
        "imported_session_summary" => truncate_for_compact(&format!("summary: {first_line}"), 80),
        "imported_user_prompt" => truncate_for_compact(&format!("prompt: {first_line}"), 80),
        _ => truncate_for_compact(first_line, 80),
    }
}

fn truncate_for_compact(input: &str, max_chars: usize) -> String {
    let trimmed = input.trim();
    let count = trimmed.chars().count();
    if count <= max_chars {
        return trimmed.to_string();
    }
    let prefix: String = trimmed.chars().take(max_chars.saturating_sub(3)).collect();
    format!("{prefix}...")
}

fn stable_handle(prefix: &str, key: &str) -> String {
    let mut hasher = blake3::Hasher::new();
    hasher.update(prefix.as_bytes());
    hasher.update(&[0]);
    hasher.update(key.as_bytes());
    let hex = hasher.finalize().to_hex();
    format!("{prefix}-{}", &hex[..10])
}

fn content_hash<T: Serialize>(value: &T) -> Result<String> {
    let bytes = serde_json::to_vec(value)?;
    Ok(blake3::hash(&bytes).to_hex().to_string())
}

fn node_with_content_freshness(mut node: GraphNode) -> Result<GraphNode> {
    let mut hashable = node.clone();
    hashable.freshness = None;
    node.freshness = Some(GraphFreshness::content_hash(content_hash(&hashable)?));
    Ok(node)
}

fn edge_with_content_freshness(mut edge: GraphEdge) -> Result<GraphEdge> {
    let mut hashable = edge.clone();
    hashable.freshness = None;
    edge.freshness = Some(GraphFreshness::content_hash(content_hash(&hashable)?));
    Ok(edge)
}

#[derive(Clone, Copy)]
enum SemanticRelatedKind {
    Concept,
}

fn semantic_related_kind_name(kind: SemanticRelatedKind) -> &'static str {
    match kind {
        SemanticRelatedKind::Concept => "concept",
    }
}

fn semantic_related_command(root: &Path, query: &str, kind: SemanticRelatedKind) -> String {
    format!(
        "tsift semantic {} --path {} --kind {} --limit 10",
        shell_quote(query),
        shell_quote(root.to_string_lossy().as_ref()),
        semantic_related_kind_name(kind)
    )
}

fn semantic_embedding(input: &str) -> Vec<f64> {
    let mut vector = vec![0.0; SEMANTIC_EMBEDDING_DIM];
    let mut tokens = traversal_tokens(input);
    if tokens.is_empty() {
        let trimmed = input.trim().to_ascii_lowercase();
        if !trimmed.is_empty() {
            tokens.insert(trimmed);
        }
    }

    for token in tokens {
        let hash = blake3::hash(token.as_bytes());
        let bytes = hash.as_bytes();
        let idx = usize::from(bytes[0]) % SEMANTIC_EMBEDDING_DIM;
        let sign = if bytes[1] & 1 == 0 { 1.0 } else { -1.0 };
        vector[idx] += sign;
    }

    let norm = vector.iter().map(|value| value * value).sum::<f64>().sqrt();
    if norm > 0.0 {
        for value in &mut vector {
            *value /= norm;
        }
    }
    vector
}

fn semantic_embedding_property(input: &str) -> String {
    semantic_embedding(input)
        .iter()
        .map(|value| format!("{value:.6}"))
        .collect::<Vec<_>>()
        .join(",")
}

fn traversal_tokens(input: &str) -> BTreeSet<String> {
    input
        .split(|ch: char| !(ch.is_ascii_alphanumeric() || ch == '_' || ch == '-'))
        .flat_map(|part| part.split(['_', '-']))
        .map(str::trim)
        .filter(|part| part.len() >= 3)
        .map(|part| part.to_ascii_lowercase())
        .collect()
}

fn insert_semantic_edge(
    edge_map: &mut BTreeMap<(String, String, String), GraphEdge>,
    edge: GraphEdge,
) {
    edge_map
        .entry((edge.from_id.clone(), edge.to_id.clone(), edge.kind.clone()))
        .or_insert(edge);
}

fn shell_quote(s: &str) -> String {
    let unquoted =
        if (s.starts_with('"') && s.ends_with('"')) || (s.starts_with('\'') && s.ends_with('\'')) {
            &s[1..s.len() - 1]
        } else {
            s
        };

    if unquoted
        .chars()
        .all(|c| c.is_alphanumeric() || c == '_' || c == '-' || c == '.' || c == '/')
    {
        format!("\"{}\"", unquoted)
    } else {
        format!(
            "\"{}\"",
            unquoted.replace('\\', "\\\\").replace('"', "\\\"")
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;
    use tsift_memory::{MemoryEventKind, MemoryStore, default_memory_db_path};

    #[test]
    fn project_memory_events_links_events_to_sessions() {
        let event = MemoryEvent::new(MemoryEventKind::ResponseSummary, "session.md", "done")
            .with_session_id("session-a");
        let projection = project_memory_events(&[event]);
        assert_eq!(projection.nodes.len(), 2);
        assert_eq!(projection.edges.len(), 1);
        assert!(
            projection
                .nodes
                .iter()
                .any(|node| node.kind == "memory_session")
        );
        assert!(
            projection
                .nodes
                .iter()
                .any(|node| node.kind == "memory_event")
        );
    }

    #[test]
    fn rank_memory_events_prefers_recent_relevant_events() {
        let now = 1_700_000_000;
        let old = MemoryEvent::new(
            MemoryEventKind::ResponseSummary,
            "old",
            "graph retrieval design shipped",
        )
        .with_observed_at_unix(now - 30 * 24 * 3600);
        let recent = MemoryEvent::new(
            MemoryEventKind::ResponseSummary,
            "recent",
            "graph retrieval follow-up",
        )
        .with_observed_at_unix(now - 60);
        let config = MemoryDecayConfig {
            half_life_secs: 7.0 * 24.0 * 3600.0,
            lexical_weight: 0.5,
            recency_weight: 0.5,
        };
        let ranked = rank_memory_events(&[old, recent], "graph retrieval", now, config, 10);
        assert_eq!(ranked[0].event.source_ref, "recent");
    }

    #[test]
    fn rank_memory_events_keeps_lexical_hits_without_timestamp() {
        let now = 1_700_000_000;
        let event = MemoryEvent::new(
            MemoryEventKind::ResponseSummary,
            "untimed",
            "semantic graph memory",
        );
        let off_topic_fresh = MemoryEvent::new(
            MemoryEventKind::ResponseSummary,
            "fresh",
            "unrelated build log output",
        )
        .with_observed_at_unix(now - 10);
        let config = MemoryDecayConfig::default();
        let ranked = rank_memory_events(
            &[event.clone(), off_topic_fresh],
            "semantic graph memory",
            now,
            config,
            10,
        );
        assert_eq!(ranked[0].event.source_ref, event.source_ref);
    }

    #[test]
    fn rank_memory_event_candidates_bounds_db_candidates_before_scoring() {
        let dir = TempDir::new().unwrap();
        let memory_db = default_memory_db_path(dir.path());
        std::fs::create_dir_all(memory_db.parent().unwrap()).unwrap();
        let store = MemoryStore::open_or_create(&memory_db).unwrap();
        let now = 1_700_000_000;
        for index in 0..40 {
            store
                .insert_event(
                    &MemoryEvent::new(
                        MemoryEventKind::ResponseSummary,
                        format!("old-{index}"),
                        format!("ordinary memory event {index}"),
                    )
                    .with_observed_at_unix(now - 20_000 - index),
                )
                .unwrap();
        }
        store
            .insert_event(
                &MemoryEvent::new(
                    MemoryEventKind::ResponseSummary,
                    "needle",
                    "semantic needle graph retrieval",
                )
                .with_observed_at_unix(now - 30_000),
            )
            .unwrap();
        store
            .insert_event(
                &MemoryEvent::new(
                    MemoryEventKind::ResponseSummary,
                    "recent",
                    "fresh unrelated release note",
                )
                .with_observed_at_unix(now - 10),
            )
            .unwrap();

        assert_eq!(memory_rank_candidate_limit(2), 16);
        let ranked = rank_memory_event_candidates(
            &memory_db,
            "semantic needle",
            now,
            MemoryDecayConfig::default(),
            2,
        )
        .unwrap();
        assert_eq!(ranked.len(), 2);
        assert!(
            ranked
                .iter()
                .any(|scored| scored.event.source_ref == "needle")
        );
        assert!(
            ranked
                .iter()
                .any(|scored| scored.event.source_ref == "recent")
        );
    }

    #[test]
    fn plan_memory_query_carries_default_decay_config() {
        let plan = plan_memory_query("graph rag", 5, 1500).unwrap();
        assert_eq!(plan.decay, MemoryDecayConfig::default());
        assert_eq!(plan.candidate_limit, 40);
        assert!(
            plan.output_contract
                .iter()
                .any(|contract| contract.contains("candidate set capped before ranking"))
        );
        assert!(
            plan.next_commands
                .iter()
                .any(|cmd| cmd.contains("project-graph"))
        );
    }

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
        let report = project_memory_into_graph(&memory_db, &graph_db, 100).unwrap();
        assert_eq!(report.events_projected, 2);
        assert!(
            report.nodes_upserted >= 3,
            "two events + one session node, got {}",
            report.nodes_upserted
        );
        assert!(
            report.edges_upserted >= 2,
            "session records each event, got {}",
            report.edges_upserted
        );

        let conn = rusqlite::Connection::open(&graph_db).unwrap();
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

    #[test]
    fn traversal_projection_adds_semantic_memory_rows() {
        let dir = TempDir::new().unwrap();
        let event = MemoryEvent::new(
            MemoryEventKind::ResponseSummary,
            "session.md",
            "semantic memory graph",
        )
        .with_session_id("sess-1")
        .with_observed_at_unix(1_700_000_000);
        let mut nodes = Vec::new();
        let mut edges = Vec::new();
        append_memory_events_as_traversal_rows(dir.path(), &[event], &mut nodes, &mut edges)
            .unwrap();

        assert!(nodes.iter().any(|node| node.kind == "memory_event"));
        assert!(nodes.iter().any(|node| {
            node.kind == "semantic_concept"
                && node.properties.get("provider") == Some(&"tsift-memory".to_string())
        }));
        assert!(edges.iter().any(|edge| edge.kind == "mentions_concept"));
    }
}
