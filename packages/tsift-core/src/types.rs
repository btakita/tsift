use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

pub const SQLITE_GRAPH_SCHEMA_VERSION: i64 = 5;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GraphProvenance {
    pub source: String,
    pub source_ref: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub content_hash: Option<String>,
}

impl GraphProvenance {
    pub fn new(source: impl Into<String>, source_ref: impl Into<String>) -> Self {
        Self {
            source: source.into(),
            source_ref: source_ref.into(),
            content_hash: None,
        }
    }

    pub fn with_content_hash(mut self, content_hash: impl Into<String>) -> Self {
        self.content_hash = Some(content_hash.into());
        self
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GraphFreshness {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub content_hash: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub observed_at_unix: Option<i64>,
}

impl GraphFreshness {
    pub fn content_hash(content_hash: impl Into<String>) -> Self {
        Self {
            content_hash: Some(content_hash.into()),
            observed_at_unix: None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GraphNode {
    pub id: String,
    pub kind: String,
    pub label: String,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub properties: BTreeMap<String, String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub provenance: Vec<GraphProvenance>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub freshness: Option<GraphFreshness>,
}

impl GraphNode {
    pub fn new(id: impl Into<String>, kind: impl Into<String>, label: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            kind: kind.into(),
            label: label.into(),
            properties: BTreeMap::new(),
            provenance: Vec::new(),
            freshness: None,
        }
    }

    pub fn with_property(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
        self.properties.insert(key.into(), value.into());
        self
    }

    pub fn with_provenance(mut self, provenance: GraphProvenance) -> Self {
        self.provenance.push(provenance);
        self
    }

    pub fn with_freshness(mut self, freshness: GraphFreshness) -> Self {
        self.freshness = Some(freshness);
        self
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GraphEdge {
    #[serde(default)]
    pub id: String,
    pub from_id: String,
    pub to_id: String,
    pub kind: String,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub properties: BTreeMap<String, String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub provenance: Vec<GraphProvenance>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub freshness: Option<GraphFreshness>,
}

impl GraphEdge {
    pub fn stable_id(from_id: &str, to_id: &str, kind: &str) -> String {
        stable_graph_edge_id(from_id, to_id, kind)
    }

    pub fn new(
        from_id: impl Into<String>,
        to_id: impl Into<String>,
        kind: impl Into<String>,
    ) -> Self {
        let from_id = from_id.into();
        let to_id = to_id.into();
        let kind = kind.into();
        Self {
            id: stable_graph_edge_id(&from_id, &to_id, &kind),
            from_id,
            to_id,
            kind,
            properties: BTreeMap::new(),
            provenance: Vec::new(),
            freshness: None,
        }
    }

    pub fn with_property(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
        self.properties.insert(key.into(), value.into());
        self
    }

    pub fn with_provenance(mut self, provenance: GraphProvenance) -> Self {
        self.provenance.push(provenance);
        self
    }

    pub fn with_freshness(mut self, freshness: GraphFreshness) -> Self {
        self.freshness = Some(freshness);
        self
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TerseGraphNode {
    pub id: String,
    pub kind: String,
    pub label: String,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub properties: BTreeMap<String, String>,
}

impl From<GraphNode> for TerseGraphNode {
    fn from(node: GraphNode) -> Self {
        Self {
            id: node.id,
            kind: node.kind,
            label: node.label,
            properties: node.properties,
        }
    }
}

impl From<&GraphNode> for TerseGraphNode {
    fn from(node: &GraphNode) -> Self {
        Self {
            id: node.id.clone(),
            kind: node.kind.clone(),
            label: node.label.clone(),
            properties: node.properties.clone(),
        }
    }
}

impl From<TerseGraphNode> for GraphNode {
    fn from(node: TerseGraphNode) -> Self {
        Self {
            id: node.id,
            kind: node.kind,
            label: node.label,
            properties: node.properties,
            provenance: Vec::new(),
            freshness: None,
        }
    }
}

impl From<&TerseGraphNode> for GraphNode {
    fn from(node: &TerseGraphNode) -> Self {
        Self {
            id: node.id.clone(),
            kind: node.kind.clone(),
            label: node.label.clone(),
            properties: node.properties.clone(),
            provenance: Vec::new(),
            freshness: None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TerseGraphEdge {
    #[serde(default)]
    pub id: String,
    pub from_id: String,
    pub to_id: String,
    pub kind: String,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub properties: BTreeMap<String, String>,
}

impl From<GraphEdge> for TerseGraphEdge {
    fn from(edge: GraphEdge) -> Self {
        Self {
            id: edge.id,
            from_id: edge.from_id,
            to_id: edge.to_id,
            kind: edge.kind,
            properties: edge.properties,
        }
    }
}

impl From<&GraphEdge> for TerseGraphEdge {
    fn from(edge: &GraphEdge) -> Self {
        Self {
            id: edge.id.clone(),
            from_id: edge.from_id.clone(),
            to_id: edge.to_id.clone(),
            kind: edge.kind.clone(),
            properties: edge.properties.clone(),
        }
    }
}

impl From<TerseGraphEdge> for GraphEdge {
    fn from(edge: TerseGraphEdge) -> Self {
        Self {
            id: edge.id,
            from_id: edge.from_id,
            to_id: edge.to_id,
            kind: edge.kind,
            properties: edge.properties,
            provenance: Vec::new(),
            freshness: None,
        }
    }
}

impl From<&TerseGraphEdge> for GraphEdge {
    fn from(edge: &TerseGraphEdge) -> Self {
        Self {
            id: edge.id.clone(),
            from_id: edge.from_id.clone(),
            to_id: edge.to_id.clone(),
            kind: edge.kind.clone(),
            properties: edge.properties.clone(),
            provenance: Vec::new(),
            freshness: None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct TerseGraphSubgraph {
    pub nodes: Vec<TerseGraphNode>,
    pub edges: Vec<TerseGraphEdge>,
}

impl From<GraphSubgraph> for TerseGraphSubgraph {
    fn from(subgraph: GraphSubgraph) -> Self {
        Self {
            nodes: subgraph
                .nodes
                .into_iter()
                .map(TerseGraphNode::from)
                .collect(),
            edges: subgraph
                .edges
                .into_iter()
                .map(TerseGraphEdge::from)
                .collect(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TerseSearchHit {
    pub artifact_id: String,
    pub confidence: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub location: Option<String>,
    pub path: String,
    pub rank: usize,
    pub score: f64,
    pub snippet: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TerseHealthScore {
    pub name: String,
    pub overall: f64,
}

pub fn stable_graph_edge_id(from_id: &str, to_id: &str, kind: &str) -> String {
    let raw = serde_json::json!([from_id, kind, to_id]).to_string();
    format!("edge:{}", blake3::hash(raw.as_bytes()).to_hex())
}

pub fn graph_edge_id(edge: &GraphEdge) -> String {
    if edge.id.is_empty() {
        stable_graph_edge_id(&edge.from_id, &edge.to_id, &edge.kind)
    } else {
        edge.id.clone()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct GraphProjection {
    pub nodes: Vec<GraphNode>,
    pub edges: Vec<GraphEdge>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GraphPath {
    pub nodes: Vec<String>,
    pub hops: usize,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GraphSubgraph {
    pub nodes: Vec<GraphNode>,
    pub edges: Vec<GraphEdge>,
}

impl GraphSubgraph {
    pub fn sorted(mut self) -> Self {
        self.nodes.sort_by(|left, right| left.id.cmp(&right.id));
        self.edges.sort_by(|left, right| {
            left.from_id
                .cmp(&right.from_id)
                .then(left.kind.cmp(&right.kind))
                .then(left.to_id.cmp(&right.to_id))
                .then_with(|| graph_edge_id(left).cmp(&graph_edge_id(right)))
        });
        self
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GraphPropertyFilter {
    pub key: String,
    pub value: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct GraphQueryOptions {
    pub cursor: Option<String>,
    pub limit: Option<usize>,
    pub property_filters: Vec<GraphPropertyFilter>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NeighborhoodScoring {
    #[default]
    BreadthFirst,
    EdgeKindWeighted,
    DegreeWeighted,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PropertyMode {
    Full,
    Sample,
    Omit,
}

impl Default for PropertyMode {
    fn default() -> Self {
        Self::Full
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RankedNeighborhoodOptions {
    pub depth: usize,
    pub max_nodes: usize,
    pub scoring: NeighborhoodScoring,
    #[serde(default)]
    pub edge_kind: Option<String>,
    #[serde(default)]
    pub property_mode: PropertyMode,
}

impl RankedNeighborhoodOptions {
    pub fn new(depth: usize, max_nodes: usize) -> Self {
        Self {
            depth,
            max_nodes,
            scoring: NeighborhoodScoring::BreadthFirst,
            edge_kind: None,
            property_mode: PropertyMode::Full,
        }
    }

    pub fn with_scoring(mut self, scoring: NeighborhoodScoring) -> Self {
        self.scoring = scoring;
        self
    }

    pub fn with_edge_kind(mut self, kind: impl Into<String>) -> Self {
        self.edge_kind = Some(kind.into());
        self
    }

    pub fn with_property_mode(mut self, mode: PropertyMode) -> Self {
        self.property_mode = mode;
        self
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RankedNeighborhoodResult {
    pub nodes: Vec<GraphNode>,
    pub edges: Vec<GraphEdge>,
    pub pruned_count: usize,
    pub total_discovered: usize,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct GraphQueryPage {
    pub cursor: Option<String>,
    pub limit: Option<usize>,
    pub next_cursor: Option<String>,
    pub returned_nodes: usize,
    pub returned_edges: usize,
    pub truncated: bool,
    pub diagnostics: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GraphPagedSubgraph {
    pub nodes: Vec<GraphNode>,
    pub edges: Vec<GraphEdge>,
    pub page: GraphQueryPage,
}
