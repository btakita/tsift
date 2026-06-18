//! Read-only Knowledge Graph evidence lookup for agent-doc (#kgadactivate).
//!
//! Realizes the `tsift-agent-doc` side of the Local KG Model Contract in
//! [`../../specs/local-kg-model.md`](../../specs/local-kg-model.md) lines 29-30:
//! agent-doc may READ `.tsift/graph.db` evidence via the tsift-sqlite/tsift-core
//! `GraphStore` layer, but must not own a separate extraction pipeline or
//! local-model lifecycle. This module is the read seam; it produces typed
//! `GraphEvidenceReport` snapshots that planning/orchestration callers (or the
//! `tsift kg evidence` CLI) can consume without coupling to extraction.
//!
//! Design notes:
//! - Read-only via `SqliteGraphStore::open_read_only_resilient`. Missing db is
//!   reported as `exists: false` rather than an error — callers must tolerate
//!   workspaces that have not yet run KG extraction.
//! - Substring + kind filters run in Rust over `all_nodes()` so the agent-doc
//!   crate stays independent of store-specific SQL dialects. The store layer's
//!   own query helpers (`outgoing_edges`, `incident_edges`) are available for
//!   deeper traversals, but the evidence lookup intentionally stays bounded so
//!   a single call is cheap enough to run on every planning cycle.
//! - Node ranking is by incident edge count (a cheap proxy for "how connected
//!   is this entity"), then label alphabetical for determinism.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::Serialize;

use tsift_core::{GraphNode, GraphStore};

/// Relative location of the KG graph store under a project root.
pub const DEFAULT_GRAPH_DB_RELATIVE: &str = ".tsift/graph.db";

/// Cap on returned evidence nodes when the caller does not specify `limit`.
pub const DEFAULT_EVIDENCE_LIMIT: usize = 20;

/// Default cap on total `graph_nodes` before bounded evidence scanning is
/// skipped. The evidence lookup loads every node into memory to rank by
/// connectivity (see [`build_report`]); on large workspace graphs (hundreds of
/// thousands of nodes) that is too expensive to run on every planning/digest
/// cycle. [`read_graph_evidence_bounded`] checks the cheap `graph_counts()`
/// first and returns a `scanned: false` report when the store exceeds this cap,
/// so callers still learn the KG exists without paying the full scan.
pub const DEFAULT_EVIDENCE_MAX_SCAN_NODES: usize = 50_000;

/// Query parameters for an evidence lookup. All fields optional except
/// `limit` (defaults to [`DEFAULT_EVIDENCE_LIMIT`]).
#[derive(Debug, Clone, Serialize)]
pub struct GraphEvidenceQuery {
    /// Substring matched case-insensitively against node `label`, `id`, and
    /// `kind`. `None` means "no symbol filter" (return top-K by connectivity).
    pub symbol: Option<String>,
    /// Restrict matches to a single node `kind` (e.g. `kg_source`, `concept`).
    pub kind: Option<String>,
    /// Maximum number of matched nodes to return. Always ≥ 1.
    pub limit: usize,
}

impl Default for GraphEvidenceQuery {
    fn default() -> Self {
        Self {
            symbol: None,
            kind: None,
            limit: DEFAULT_EVIDENCE_LIMIT,
        }
    }
}

impl GraphEvidenceQuery {
    /// Builder: set the symbol substring.
    pub fn with_symbol(mut self, symbol: impl Into<String>) -> Self {
        let symbol = symbol.into();
        self.symbol = if symbol.trim().is_empty() {
            None
        } else {
            Some(symbol)
        };
        self
    }

    /// Builder: restrict to a node kind.
    pub fn with_kind(mut self, kind: impl Into<String>) -> Self {
        let kind = kind.into();
        self.kind = if kind.trim().is_empty() {
            None
        } else {
            Some(kind)
        };
        self
    }

    /// Builder: cap the number of returned nodes.
    pub fn with_limit(mut self, limit: usize) -> Self {
        self.limit = limit.max(1);
        self
    }
}

/// One matched node in the evidence report. Provenance fields are flattened
/// out of `GraphNode::provenance` for easy caller consumption — agents want
/// `source_ref` and `source_system` directly, not a nested provenance chain.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct GraphEvidenceNode {
    pub id: String,
    pub kind: String,
    pub label: String,
    pub source_refs: Vec<String>,
    pub provenance_systems: Vec<String>,
    /// Number of edges in the store that reference this node id on either
    /// side. Cheap connectivity proxy used for ranking.
    pub incident_edge_count: usize,
}

impl GraphEvidenceNode {
    /// Project a `GraphNode` into an evidence row, looking up its connectivity
    /// from the precomputed edge-count map.
    pub fn from_graph_node(node: &GraphNode, edge_counts: &HashMap<&str, usize>) -> Self {
        Self {
            id: node.id.clone(),
            kind: node.kind.clone(),
            label: node.label.clone(),
            source_refs: node
                .provenance
                .iter()
                .map(|p| p.source_ref.clone())
                .collect(),
            provenance_systems: node.provenance.iter().map(|p| p.source.clone()).collect(),
            incident_edge_count: edge_counts.get(node.id.as_str()).copied().unwrap_or(0),
        }
    }
}

/// Result of an evidence lookup. Always returned (errors only for unreadable
/// stores); a missing graph.db produces `exists: false` so callers can branch
/// without try/catching.
#[derive(Debug, Clone, Serialize)]
pub struct GraphEvidenceReport {
    pub graph_db: String,
    pub exists: bool,
    /// Whether the full node/edge scan ran. `false` means either the store was
    /// missing (`exists: false`) or it exceeded the bounded-scan cap (see
    /// [`read_graph_evidence_bounded`]); in the latter case `total_nodes_in_db`
    /// / `total_edges_in_db` are still populated from the cheap `graph_counts()`
    /// query but `matched_nodes` is empty.
    pub scanned: bool,
    pub total_nodes_in_db: usize,
    pub total_edges_in_db: usize,
    pub query: GraphEvidenceQuery,
    pub matched_nodes: Vec<GraphEvidenceNode>,
}

/// Resolve the graph.db under a project root and read evidence. Equivalent to
/// `read_graph_evidence_from_db(project_root.join(".tsift/graph.db"), query)`.
pub fn read_graph_evidence(
    project_root: &Path,
    query: &GraphEvidenceQuery,
) -> Result<GraphEvidenceReport> {
    let db_path: PathBuf = project_root.join(DEFAULT_GRAPH_DB_RELATIVE);
    read_graph_evidence_from_db(&db_path, query)
}

/// Read evidence from an explicit graph.db path. The canonical entry point —
/// CLI callers pass `--graph-db` here directly; library callers usually go
/// through `read_graph_evidence` with a project root.
pub fn read_graph_evidence_from_db(
    db_path: &Path,
    query: &GraphEvidenceQuery,
) -> Result<GraphEvidenceReport> {
    let db_display = db_path.display().to_string();
    if !db_path.exists() {
        return Ok(GraphEvidenceReport {
            graph_db: db_display,
            exists: false,
            scanned: false,
            total_nodes_in_db: 0,
            total_edges_in_db: 0,
            query: clone_query(query),
            matched_nodes: Vec::new(),
        });
    }

    let store = tsift_sqlite::SqliteGraphStore::open_read_only_resilient(db_path)
        .with_context(|| format!("opening graph.db read-only at {}", db_path.display()))?;
    let all_nodes = store
        .all_nodes()
        .with_context(|| "reading graph_nodes for kg evidence")?;
    let all_edges = store
        .all_edges()
        .unwrap_or_default();

    let report = build_report(db_display, &all_nodes, &all_edges, query);
    Ok(report)
}

/// Read evidence with a node-count guard so large workspace graphs do not pay
/// the full `all_nodes()` / `all_edges()` scan on every call. Intended for the
/// planning/session-digest hot path (#kgwiring) where the lookup runs on every
/// cycle but the graph may hold hundreds of thousands of nodes.
///
/// Behavior:
/// - Missing store → `exists: false`, `scanned: false` (same as
///   [`read_graph_evidence_from_db`]).
/// - Store present but `total_nodes > max_scan_nodes` → `exists: true`,
///   `scanned: false`, totals from the cheap `graph_counts()` query, and an
///   empty `matched_nodes` (the caller learns the KG exists without the scan).
/// - Otherwise → full scan via [`build_report`], `scanned: true`.
pub fn read_graph_evidence_bounded(
    db_path: &Path,
    query: &GraphEvidenceQuery,
    max_scan_nodes: usize,
) -> Result<GraphEvidenceReport> {
    let db_display = db_path.display().to_string();
    if !db_path.exists() {
        return Ok(GraphEvidenceReport {
            graph_db: db_display,
            exists: false,
            scanned: false,
            total_nodes_in_db: 0,
            total_edges_in_db: 0,
            query: clone_query(query),
            matched_nodes: Vec::new(),
        });
    }

    let store = tsift_sqlite::SqliteGraphStore::open_read_only_resilient(db_path)
        .with_context(|| format!("opening graph.db read-only at {}", db_path.display()))?;
    let (total_nodes, total_edges) = store
        .graph_counts()
        .with_context(|| "reading graph counts for bounded kg evidence")?;

    if total_nodes > max_scan_nodes {
        return Ok(GraphEvidenceReport {
            graph_db: db_display,
            exists: true,
            scanned: false,
            total_nodes_in_db: total_nodes,
            total_edges_in_db: total_edges,
            query: clone_query(query),
            matched_nodes: Vec::new(),
        });
    }

    let all_nodes = store
        .all_nodes()
        .with_context(|| "reading graph_nodes for bounded kg evidence")?;
    let all_edges = store.all_edges().unwrap_or_default();
    Ok(build_report(db_display, &all_nodes, &all_edges, query))
}

/// Pure projection from loaded nodes/edges to an evidence report. Separated
/// from `read_graph_evidence_from_db` so unit tests can drive it without a
/// tempfile or sqlite — just pass synthetic node/edge vecs.
pub fn build_report(
    db_display: String,
    nodes: &[GraphNode],
    edges: &[tsift_core::GraphEdge],
    query: &GraphEvidenceQuery,
) -> GraphEvidenceReport {
    let total_nodes = nodes.len();
    let total_edges = edges.len();

    let mut edge_counts: HashMap<&str, usize> = HashMap::new();
    for edge in edges {
        *edge_counts.entry(edge.from_id.as_str()).or_insert(0) += 1;
        *edge_counts.entry(edge.to_id.as_str()).or_insert(0) += 1;
    }

    let needle = query
        .symbol
        .as_deref()
        .map(|s| s.trim().to_lowercase())
        .filter(|s| !s.is_empty());
    let kind_filter = query
        .kind
        .as_deref()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty());

    let limit = query.limit.max(1);

    let mut matched: Vec<&GraphNode> = nodes
        .iter()
        .filter(|node| {
            if let Some(kind) = &kind_filter
                && node.kind != *kind
            {
                return false;
            }
            if let Some(needle) = &needle
                && !node.label.to_lowercase().contains(needle)
                && !node.id.to_lowercase().contains(needle)
                && !node.kind.to_lowercase().contains(needle)
            {
                return false;
            }
            true
        })
        .collect();

    matched.sort_by(|a, b| {
        let count_a = edge_counts.get(a.id.as_str()).copied().unwrap_or(0);
        let count_b = edge_counts.get(b.id.as_str()).copied().unwrap_or(0);
        count_b
            .cmp(&count_a)
            .then_with(|| a.label.cmp(&b.label))
    });

    let matched_nodes: Vec<GraphEvidenceNode> = matched
        .into_iter()
        .take(limit)
        .map(|node| GraphEvidenceNode::from_graph_node(node, &edge_counts))
        .collect();

    GraphEvidenceReport {
        graph_db: db_display,
        exists: true,
        scanned: true,
        total_nodes_in_db: total_nodes,
        total_edges_in_db: total_edges,
        query: clone_query(query),
        matched_nodes,
    }
}

fn clone_query(query: &GraphEvidenceQuery) -> GraphEvidenceQuery {
    GraphEvidenceQuery {
        symbol: query.symbol.clone(),
        kind: query.kind.clone(),
        limit: query.limit,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;
    use tsift_core::{GraphEdge, GraphProjection, GraphProvenance};
    use tsift_sqlite::SqliteGraphStore;

    fn sample_nodes() -> Vec<GraphNode> {
        vec![
            GraphNode::new("n:kg-1", "kg_source", "tsift-kg")
                .with_property("provider", "tsift-kg")
                .with_provenance(GraphProvenance::new("tsift-kg", "spec.md")),
            GraphNode::new("n:kg-2", "kg_source", "OllamaKgExtractor")
                .with_provenance(GraphProvenance::new("tsift-kg", "ollama.rs")),
            GraphNode::new("n:other", "concept", "lease"),
            GraphNode::new("n:lease", "concept", "GPU lease registry"),
            GraphNode::new("n:json", "format", "JSON"),
        ]
    }

    fn sample_edges() -> Vec<GraphEdge> {
        vec![
            GraphEdge::new("n:kg-1", "n:kg-2", "calls"),
            GraphEdge::new("n:kg-1", "n:other", "related_to"),
            GraphEdge::new("n:kg-2", "n:json", "emits"),
            // n:lease has no edges on purpose — exercises the zero-connectivity rank.
        ]
    }

    #[test]
    fn build_report_ranks_by_incident_edge_count_desc_then_label() {
        let report = build_report(
            "test.db".to_string(),
            &sample_nodes(),
            &sample_edges(),
            &GraphEvidenceQuery::default(),
        );
        assert_eq!(report.matched_nodes.len(), 5);
        // n:kg-1 has 2 incident edges (calls n:kg-2, related_to n:other);
        // n:kg-2 has 2 incident edges (called by n:kg-1, emits n:json).
        // Tie broken by ascending label byte order: 'O' (0x4F) < 't' (0x74),
        // so "OllamaKgExtractor" (n:kg-2) sorts before "tsift-kg" (n:kg-1).
        assert_eq!(report.matched_nodes[0].id, "n:kg-2");
        assert_eq!(report.matched_nodes[0].incident_edge_count, 2);
        assert_eq!(report.matched_nodes[1].id, "n:kg-1");
        assert_eq!(report.matched_nodes[1].incident_edge_count, 2);
        // n:lease comes last (0 edges).
        assert_eq!(report.matched_nodes[4].id, "n:lease");
        assert_eq!(report.matched_nodes[4].incident_edge_count, 0);
    }

    #[test]
    fn build_report_symbol_filter_matches_label_case_insensitively() {
        let report = build_report(
            "test.db".to_string(),
            &sample_nodes(),
            &sample_edges(),
            &GraphEvidenceQuery::default().with_symbol("lease"),
        );
        // Matches label "GPU lease registry" and label "lease". NOT "tsift-kg".
        let labels: Vec<&str> = report
            .matched_nodes
            .iter()
            .map(|n| n.label.as_str())
            .collect();
        assert!(labels.contains(&"GPU lease registry"));
        assert!(labels.contains(&"lease"));
        assert!(!labels.contains(&"tsift-kg"));
    }

    #[test]
    fn build_report_symbol_filter_falls_back_to_id_and_kind() {
        // "kg_source" is a kind; a symbol search should pick up nodes by kind
        // even when their label does not contain the substring.
        let report = build_report(
            "test.db".to_string(),
            &sample_nodes(),
            &sample_edges(),
            &GraphEvidenceQuery::default().with_symbol("kg_source"),
        );
        assert_eq!(report.matched_nodes.len(), 2);
        assert!(report
            .matched_nodes
            .iter()
            .all(|n| n.kind == "kg_source"));
    }

    #[test]
    fn build_report_kind_filter_restricts_to_exact_kind_match() {
        let report = build_report(
            "test.db".to_string(),
            &sample_nodes(),
            &sample_edges(),
            &GraphEvidenceQuery::default().with_kind("concept"),
        );
        assert_eq!(report.matched_nodes.len(), 2);
        assert!(report.matched_nodes.iter().all(|n| n.kind == "concept"));
    }

    #[test]
    fn build_report_limit_caps_result_count() {
        let report = build_report(
            "test.db".to_string(),
            &sample_nodes(),
            &sample_edges(),
            &GraphEvidenceQuery::default().with_limit(2),
        );
        assert_eq!(report.matched_nodes.len(), 2);
        // Limit does not change total counts.
        assert_eq!(report.total_nodes_in_db, 5);
        assert_eq!(report.total_edges_in_db, 3);
    }

    #[test]
    fn build_report_provenance_flattened_into_source_refs_and_systems() {
        let report = build_report(
            "test.db".to_string(),
            &sample_nodes(),
            &sample_edges(),
            &GraphEvidenceQuery::default().with_symbol("tsift-kg"),
        );
        let node = report
            .matched_nodes
            .iter()
            .find(|n| n.id == "n:kg-1")
            .expect("n:kg-1 matches symbol");
        assert_eq!(node.source_refs, vec!["spec.md".to_string()]);
        assert_eq!(node.provenance_systems, vec!["tsift-kg".to_string()]);
    }

    #[test]
    fn build_report_empty_db_returns_zero_totals() {
        let report = build_report(
            "empty.db".to_string(),
            &[],
            &[],
            &GraphEvidenceQuery::default(),
        );
        assert!(report.exists);
        assert_eq!(report.total_nodes_in_db, 0);
        assert_eq!(report.total_edges_in_db, 0);
        assert!(report.matched_nodes.is_empty());
    }

    #[test]
    fn read_graph_evidence_from_db_returns_exists_false_for_missing_path() {
        let dir = TempDir::new().unwrap();
        let missing = dir.path().join("never-created.db");
        let report = read_graph_evidence_from_db(&missing, &GraphEvidenceQuery::default())
            .expect("missing db should not error");
        assert!(!report.exists);
        assert_eq!(report.total_nodes_in_db, 0);
        assert!(report.matched_nodes.is_empty());
    }

    #[test]
    fn read_graph_evidence_from_db_reads_populated_store() {
        let dir = TempDir::new().unwrap();
        let db_path = dir.path().join("graph.db");
        let mut store = SqliteGraphStore::open(&db_path).unwrap();
        let mut projection = GraphProjection::default();
        projection.nodes.push(
            GraphNode::new("n:kg-1", "kg_source", "tsift-kg")
                .with_provenance(GraphProvenance::new("tsift-kg", "spec.md")),
        );
        projection
            .nodes
            .push(GraphNode::new("n:other", "concept", "lease"));
        projection
            .edges
            .push(GraphEdge::new("n:kg-1", "n:other", "related_to"));
        store.upsert_projection(&projection).unwrap();
        // Drop the writable handle so the read-only open is clean.
        drop(store);

        let report = read_graph_evidence_from_db(
            &db_path,
            &GraphEvidenceQuery::default().with_kind("kg_source"),
        )
        .expect("populated db reads succeed");
        assert!(report.exists);
        assert_eq!(report.total_nodes_in_db, 2);
        assert_eq!(report.total_edges_in_db, 1);
        assert_eq!(report.matched_nodes.len(), 1);
        assert_eq!(report.matched_nodes[0].id, "n:kg-1");
        assert_eq!(report.matched_nodes[0].source_refs, vec!["spec.md"]);
    }

    #[test]
    fn build_report_sets_scanned_true() {
        let report = build_report(
            "test.db".to_string(),
            &sample_nodes(),
            &sample_edges(),
            &GraphEvidenceQuery::default(),
        );
        assert!(report.scanned);
    }

    #[test]
    fn read_graph_evidence_bounded_missing_db_is_unscanned() {
        let dir = TempDir::new().unwrap();
        let missing = dir.path().join("never-created.db");
        let report = read_graph_evidence_bounded(
            &missing,
            &GraphEvidenceQuery::default(),
            DEFAULT_EVIDENCE_MAX_SCAN_NODES,
        )
        .expect("missing db should not error");
        assert!(!report.exists);
        assert!(!report.scanned);
        assert!(report.matched_nodes.is_empty());
    }

    #[test]
    fn read_graph_evidence_bounded_scans_small_db() {
        let dir = TempDir::new().unwrap();
        let db_path = dir.path().join("graph.db");
        let mut store = SqliteGraphStore::open(&db_path).unwrap();
        let mut projection = GraphProjection::default();
        projection
            .nodes
            .push(GraphNode::new("n:kg-1", "kg_source", "tsift-kg"));
        projection
            .nodes
            .push(GraphNode::new("n:other", "concept", "lease"));
        projection
            .edges
            .push(GraphEdge::new("n:kg-1", "n:other", "related_to"));
        store.upsert_projection(&projection).unwrap();
        drop(store);

        let report = read_graph_evidence_bounded(
            &db_path,
            &GraphEvidenceQuery::default(),
            DEFAULT_EVIDENCE_MAX_SCAN_NODES,
        )
        .expect("small db scans");
        assert!(report.exists);
        assert!(report.scanned);
        assert_eq!(report.total_nodes_in_db, 2);
        assert_eq!(report.matched_nodes.len(), 2);
    }

    #[test]
    fn read_graph_evidence_bounded_skips_oversized_db() {
        let dir = TempDir::new().unwrap();
        let db_path = dir.path().join("graph.db");
        let mut store = SqliteGraphStore::open(&db_path).unwrap();
        let mut projection = GraphProjection::default();
        for i in 0..5 {
            projection
                .nodes
                .push(GraphNode::new(format!("n:{i}"), "concept", format!("c{i}")));
        }
        store.upsert_projection(&projection).unwrap();
        drop(store);

        // Cap of 2 < 5 nodes → totals reported from graph_counts, no scan.
        let report = read_graph_evidence_bounded(&db_path, &GraphEvidenceQuery::default(), 2)
            .expect("oversized db reports counts without scanning");
        assert!(report.exists);
        assert!(!report.scanned);
        assert_eq!(report.total_nodes_in_db, 5);
        assert!(report.matched_nodes.is_empty());
    }

    #[test]
    fn query_builders_treat_blank_strings_as_no_filter() {
        let q = GraphEvidenceQuery::default()
            .with_symbol("   ")
            .with_kind("")
            .with_limit(0);
        assert!(q.symbol.is_none());
        assert!(q.kind.is_none());
        assert_eq!(q.limit, 1); // .with_limit clamps to ≥ 1
    }
}
