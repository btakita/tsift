use anyhow::{Result, bail};
use serde::{Deserialize, Serialize};
use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet, VecDeque};

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

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct TerseGraphSubgraph {
    pub nodes: Vec<TerseGraphNode>,
    pub edges: Vec<TerseGraphEdge>,
}

impl From<GraphSubgraph> for TerseGraphSubgraph {
    fn from(subgraph: GraphSubgraph) -> Self {
        Self {
            nodes: subgraph.nodes.into_iter().map(TerseGraphNode::from).collect(),
            edges: subgraph.edges.into_iter().map(TerseGraphEdge::from).collect(),
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

impl GraphProjection {
    pub fn upsert_into<S: GraphStore + ?Sized>(&self, store: &S) -> Result<()> {
        for node in &self.nodes {
            store.upsert_node(node)?;
        }
        for edge in &self.edges {
            store.upsert_edge(edge)?;
        }
        Ok(())
    }

    pub fn to_convex_rows(&self) -> ConvexProjectionRows {
        ConvexProjectionRows::from(self)
    }
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

fn graph_node_matches_filters(node: &GraphNode, filters: &[GraphPropertyFilter]) -> bool {
    filters
        .iter()
        .all(|filter| node.properties.get(&filter.key) == Some(&filter.value))
}

fn graph_edge_matches_filters(edge: &GraphEdge, filters: &[GraphPropertyFilter]) -> bool {
    filters
        .iter()
        .all(|filter| edge.properties.get(&filter.key) == Some(&filter.value))
}

pub fn apply_graph_query_page(
    mut nodes: Vec<GraphNode>,
    mut edges: Vec<GraphEdge>,
    options: GraphQueryOptions,
    mut diagnostics: Vec<String>,
) -> GraphPagedSubgraph {
    nodes.sort_by(|left, right| left.id.cmp(&right.id));
    edges.sort_by(|left, right| {
        left.from_id
            .cmp(&right.from_id)
            .then(left.kind.cmp(&right.kind))
            .then(left.to_id.cmp(&right.to_id))
    });

    let before_filter = nodes.len();
    if !options.property_filters.is_empty() {
        nodes.retain(|node| graph_node_matches_filters(node, &options.property_filters));
    }
    let after_filter = nodes.len();

    if let Some(cursor) = &options.cursor {
        nodes.retain(|node| node.id > *cursor);
    }

    let before_limit = nodes.len();
    let mut next_cursor = None;
    if let Some(limit) = options.limit
        && nodes.len() > limit
    {
        next_cursor = nodes
            .get(limit.saturating_sub(1))
            .map(|node| node.id.clone());
        nodes.truncate(limit);
    }

    let node_ids = nodes
        .iter()
        .map(|node| node.id.as_str())
        .collect::<BTreeSet<_>>();
    edges.retain(|edge| {
        node_ids.contains(edge.from_id.as_str()) && node_ids.contains(edge.to_id.as_str())
    });

    if after_filter != before_filter {
        diagnostics.push(format!(
            "property filters removed {} node(s)",
            before_filter.saturating_sub(after_filter)
        ));
    }
    if options.cursor.is_some() {
        diagnostics.push("cursor is exclusive and ordered by node id".to_string());
    }
    if next_cursor.is_some() {
        diagnostics.push(
            "result was truncated; pass page.next_cursor as --cursor for the next page".to_string(),
        );
    }

    GraphPagedSubgraph {
        page: GraphQueryPage {
            cursor: options.cursor,
            limit: options.limit,
            next_cursor,
            returned_nodes: nodes.len(),
            returned_edges: edges.len(),
            truncated: options.limit.is_some_and(|limit| before_limit > limit),
            diagnostics,
        },
        nodes,
        edges,
    }
}

pub fn apply_graph_edge_query_page(
    mut edges: Vec<GraphEdge>,
    options: GraphQueryOptions,
    mut diagnostics: Vec<String>,
) -> GraphPagedSubgraph {
    edges.sort_by_key(graph_edge_id);

    let before_filter = edges.len();
    if !options.property_filters.is_empty() {
        edges.retain(|edge| graph_edge_matches_filters(edge, &options.property_filters));
    }
    let after_filter = edges.len();

    if let Some(cursor) = &options.cursor {
        edges.retain(|edge| graph_edge_id(edge) > *cursor);
    }

    let before_limit = edges.len();
    let mut next_cursor = None;
    if let Some(limit) = options.limit
        && edges.len() > limit
    {
        next_cursor = edges.get(limit.saturating_sub(1)).map(graph_edge_id);
        edges.truncate(limit);
    }

    if after_filter != before_filter {
        diagnostics.push(format!(
            "edge property filters removed {} edge(s)",
            before_filter.saturating_sub(after_filter)
        ));
    }
    if options.cursor.is_some() {
        diagnostics.push("cursor is exclusive and ordered by edge id".to_string());
    }
    if next_cursor.is_some() {
        diagnostics.push(
            "result was truncated; pass page.next_cursor as --cursor for the next page".to_string(),
        );
    }

    GraphPagedSubgraph {
        page: GraphQueryPage {
            cursor: options.cursor,
            limit: options.limit,
            next_cursor,
            returned_nodes: 0,
            returned_edges: edges.len(),
            truncated: options.limit.is_some_and(|limit| before_limit > limit),
            diagnostics,
        },
        nodes: Vec::new(),
        edges,
    }
}

pub trait GraphStore {
    fn upsert_node(&self, node: &GraphNode) -> Result<()>;
    fn upsert_edge(&self, edge: &GraphEdge) -> Result<()>;
    fn delete_node(&self, id: &str) -> Result<usize>;
    fn delete_edge(&self, from_id: &str, to_id: &str, kind: &str) -> Result<usize>;
    fn node(&self, id: &str) -> Result<Option<GraphNode>>;
    fn all_nodes(&self) -> Result<Vec<GraphNode>>;
    fn all_edges(&self) -> Result<Vec<GraphEdge>>;
    fn edge(&self, edge_id: &str) -> Result<Option<GraphEdge>> {
        Ok(self
            .all_edges()?
            .into_iter()
            .find(|edge| graph_edge_id(edge) == edge_id))
    }
    fn graph_counts(&self) -> Result<(usize, usize)> {
        Ok((self.all_nodes()?.len(), self.all_edges()?.len()))
    }
    fn sample_edge(&self, kind: Option<&str>) -> Result<Option<GraphEdge>> {
        let mut edges = self
            .all_edges()?
            .into_iter()
            .filter(|edge| edge.from_id != edge.to_id)
            .filter(|edge| kind.is_none_or(|kind| edge.kind == kind))
            .collect::<Vec<_>>();
        edges.sort_by(|left, right| {
            left.from_id
                .cmp(&right.from_id)
                .then(left.kind.cmp(&right.kind))
                .then(left.to_id.cmp(&right.to_id))
        });
        Ok(edges.into_iter().next())
    }
    fn sample_edge_with_property(&self) -> Result<Option<(GraphEdge, GraphPropertyFilter)>> {
        let mut probes = Vec::new();
        for edge in self.all_edges()? {
            if edge.from_id == edge.to_id {
                continue;
            }
            if let Some((key, value)) = edge
                .properties
                .iter()
                .next()
                .map(|(key, value)| (key.clone(), value.clone()))
            {
                probes.push((edge, GraphPropertyFilter { key, value }));
            }
        }
        probes.sort_by(|(left_edge, left_filter), (right_edge, right_filter)| {
            left_filter
                .key
                .cmp(&right_filter.key)
                .then(left_filter.value.cmp(&right_filter.value))
                .then_with(|| graph_edge_id(left_edge).cmp(&graph_edge_id(right_edge)))
        });
        Ok(probes.into_iter().next())
    }
    fn nodes_by_kind(&self, kind: &str) -> Result<Vec<GraphNode>>;
    fn outgoing_edges(&self, from_id: &str, kind: Option<&str>) -> Result<Vec<GraphEdge>>;
    fn incident_edges(&self, node_id: &str, kind: Option<&str>) -> Result<Vec<GraphEdge>> {
        let mut edges = self
            .all_edges()?
            .into_iter()
            .filter(|edge| edge.from_id == node_id || edge.to_id == node_id)
            .filter(|edge| kind.is_none_or(|kind| edge.kind == kind))
            .collect::<Vec<_>>();
        edges.sort_by_key(graph_edge_id);
        Ok(edges)
    }
    fn paged_edges(
        &self,
        kind: Option<&str>,
        options: GraphQueryOptions,
    ) -> Result<GraphPagedSubgraph> {
        let edges = self
            .all_edges()?
            .into_iter()
            .filter(|edge| kind.is_none_or(|kind| edge.kind == kind))
            .collect::<Vec<_>>();
        Ok(apply_graph_edge_query_page(edges, options, Vec::new()))
    }
    fn paged_incident_edges(
        &self,
        node_id: &str,
        kind: Option<&str>,
        options: GraphQueryOptions,
    ) -> Result<GraphPagedSubgraph> {
        Ok(apply_graph_edge_query_page(
            self.incident_edges(node_id, kind)?,
            options,
            Vec::new(),
        ))
    }
    fn edges_between_nodes(&self, node_ids: &BTreeSet<String>) -> Result<Vec<GraphEdge>> {
        let mut edges = BTreeMap::<(String, String, String), GraphEdge>::new();
        for from_id in node_ids {
            for edge in self.outgoing_edges(from_id, None)? {
                if node_ids.contains(&edge.to_id) {
                    edges
                        .entry((edge.from_id.clone(), edge.kind.clone(), edge.to_id.clone()))
                        .or_insert(edge);
                }
            }
        }
        Ok(edges.into_values().collect())
    }
    fn shortest_path(
        &self,
        from_id: &str,
        to_id: &str,
        kind: Option<&str>,
    ) -> Result<Option<GraphPath>>;
    fn shortest_path_with_max_hops(
        &self,
        from_id: &str,
        to_id: &str,
        kind: Option<&str>,
        max_hops: Option<usize>,
    ) -> Result<Option<GraphPath>> {
        shortest_path_using_outgoing(from_id, to_id, kind, max_hops, |current, kind| {
            self.outgoing_edges(current, kind)
        })
    }
    fn neighborhood(
        &self,
        center_id: &str,
        depth: usize,
        kind: Option<&str>,
    ) -> Result<Option<GraphSubgraph>> {
        let Some(center) = self.node(center_id)? else {
            return Ok(None);
        };
        let mut nodes = BTreeMap::from([(center_id.to_string(), center)]);
        let mut edges = BTreeMap::<(String, String, String), GraphEdge>::new();
        let mut queue = VecDeque::from([(center_id.to_string(), 0usize)]);

        while let Some((current, current_depth)) = queue.pop_front() {
            if current_depth >= depth {
                continue;
            }
            let outgoing = self.outgoing_edges(&current, kind)?;
            let mut missing_ids = Vec::new();
            for edge in &outgoing {
                let edge_key = (edge.from_id.clone(), edge.kind.clone(), edge.to_id.clone());
                edges.entry(edge_key).or_insert_with(|| edge.clone());
                if !nodes.contains_key(&edge.to_id) {
                    missing_ids.push(edge.to_id.clone());
                }
            }
            if !missing_ids.is_empty() {
                missing_ids.sort();
                missing_ids.dedup();
                for id in &missing_ids {
                    if !nodes.contains_key(id) {
                        if let Some(node) = self.node(id)? {
                            nodes.insert(id.clone(), node);
                            queue.push_back((id.clone(), current_depth + 1));
                        }
                    }
                }
            }
        }

        Ok(Some(
            GraphSubgraph {
                nodes: nodes.into_values().collect(),
                edges: edges.into_values().collect(),
            }
            .sorted(),
        ))
    }
    fn paged_nodes_by_kind(
        &self,
        kind: &str,
        options: GraphQueryOptions,
    ) -> Result<GraphPagedSubgraph> {
        Ok(apply_graph_query_page(
            self.nodes_by_kind(kind)?,
            Vec::new(),
            options,
            Vec::new(),
        ))
    }
    fn paged_neighborhood(
        &self,
        center_id: &str,
        depth: usize,
        kind: Option<&str>,
        options: GraphQueryOptions,
    ) -> Result<Option<GraphPagedSubgraph>> {
        Ok(self.neighborhood(center_id, depth, kind)?.map(|subgraph| {
            apply_graph_query_page(subgraph.nodes, subgraph.edges, options, Vec::new())
        }))
    }
    fn reachable_nodes_by_kind(
        &self,
        from_id: &str,
        kind: &str,
        depth: usize,
        limit: usize,
    ) -> Result<Vec<(GraphNode, GraphPath)>> {
        let mut rows = BTreeMap::<String, (GraphNode, GraphPath)>::new();
        let mut seen = BTreeSet::from([from_id.to_string()]);
        let mut queue = VecDeque::from([(from_id.to_string(), vec![from_id.to_string()])]);

        while let Some((current, path)) = queue.pop_front() {
            let current_depth = path.len().saturating_sub(1);
            if current_depth >= depth {
                continue;
            }
            for edge in self.outgoing_edges(&current, None)? {
                if !seen.insert(edge.to_id.clone()) {
                    continue;
                }
                let Some(node) = self.node(&edge.to_id)? else {
                    continue;
                };
                let mut next_path = path.clone();
                next_path.push(edge.to_id.clone());
                let graph_path = GraphPath {
                    hops: next_path.len().saturating_sub(1),
                    nodes: next_path.clone(),
                };
                if node.kind == kind {
                    rows.entry(node.id.clone()).or_insert((node, graph_path));
                }
                queue.push_back((edge.to_id, next_path));
            }
        }
        let mut rows = rows.into_values().collect::<Vec<_>>();
        rows.sort_by(|(left_node, left_path), (right_node, right_path)| {
            left_path
                .hops
                .cmp(&right_path.hops)
                .then(left_node.label.cmp(&right_node.label))
                .then(left_node.id.cmp(&right_node.id))
        });
        if limit > 0 && rows.len() > limit {
            rows.truncate(limit);
        }
        Ok(rows)
    }

    fn reachable_nodes_by_kinds(
        &self,
        from_id: &str,
        kinds: &[&str],
        depth: usize,
        limit: usize,
    ) -> Result<BTreeMap<String, Vec<(GraphNode, GraphPath)>>> {
        let mut rows = BTreeMap::new();
        for kind in kinds {
            rows.insert(
                (*kind).to_string(),
                self.reachable_nodes_by_kind(from_id, kind, depth, limit)?,
            );
        }
        Ok(rows)
    }

    fn resolve_evidence_target(&self, target: &str, kinds: &[&str]) -> Result<Option<GraphNode>> {
        if let Some(node) = self.node(target)? {
            return Ok(Some(node));
        }
        let normalized = target.trim().trim_start_matches('#');
        for kind in kinds {
            let mut candidates = self
                .nodes_by_kind(kind)?
                .into_iter()
                .filter(|node| {
                    node.properties.get("handle").map(String::as_str) == Some(target)
                        || node.properties.get("ref_id").map(String::as_str) == Some(normalized)
                        || node.label == target
                        || node.label == format!("#{normalized}")
                })
                .collect::<Vec<_>>();
            candidates.sort_by(|left, right| left.id.cmp(&right.id));
            if let Some(candidate) = candidates.into_iter().next() {
                return Ok(Some(candidate));
            }
        }
        Ok(None)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct ConvexProjectionRows {
    pub nodes: Vec<ConvexNodeRow>,
    pub edges: Vec<ConvexEdgeRow>,
}

impl From<&GraphProjection> for ConvexProjectionRows {
    fn from(projection: &GraphProjection) -> Self {
        Self {
            nodes: projection.nodes.iter().map(ConvexNodeRow::from).collect(),
            edges: projection.edges.iter().map(ConvexEdgeRow::from).collect(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ConvexNodeRow {
    pub external_id: String,
    pub kind: String,
    pub label: String,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub properties: BTreeMap<String, String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub provenance: Vec<GraphProvenance>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub freshness: Option<GraphFreshness>,
}

impl From<&GraphNode> for ConvexNodeRow {
    fn from(node: &GraphNode) -> Self {
        Self {
            external_id: node.id.clone(),
            kind: node.kind.clone(),
            label: node.label.clone(),
            properties: node.properties.clone(),
            provenance: node.provenance.clone(),
            freshness: node.freshness.clone(),
        }
    }
}

impl From<ConvexNodeRow> for GraphNode {
    fn from(row: ConvexNodeRow) -> Self {
        Self {
            id: row.external_id,
            kind: row.kind,
            label: row.label,
            properties: row.properties,
            provenance: row.provenance,
            freshness: row.freshness,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ConvexEdgeRow {
    pub edge_key: String,
    pub from_external_id: String,
    pub to_external_id: String,
    pub kind: String,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub properties: BTreeMap<String, String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub provenance: Vec<GraphProvenance>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub freshness: Option<GraphFreshness>,
}

impl ConvexEdgeRow {
    pub fn stable_key(from_id: &str, to_id: &str, kind: &str) -> String {
        stable_graph_edge_id(from_id, to_id, kind)
    }
}

impl From<&GraphEdge> for ConvexEdgeRow {
    fn from(edge: &GraphEdge) -> Self {
        Self {
            edge_key: graph_edge_id(edge),
            from_external_id: edge.from_id.clone(),
            to_external_id: edge.to_id.clone(),
            kind: edge.kind.clone(),
            properties: edge.properties.clone(),
            provenance: edge.provenance.clone(),
            freshness: edge.freshness.clone(),
        }
    }
}

impl From<ConvexEdgeRow> for GraphEdge {
    fn from(row: ConvexEdgeRow) -> Self {
        Self {
            id: row.edge_key,
            from_id: row.from_external_id,
            to_id: row.to_external_id,
            kind: row.kind,
            properties: row.properties,
            provenance: row.provenance,
            freshness: row.freshness,
        }
    }
}

pub trait ConvexGraphClient {
    fn upsert_node_row(&self, row: &ConvexNodeRow) -> Result<()>;
    fn upsert_edge_row(&self, row: &ConvexEdgeRow) -> Result<()>;
    fn delete_node_row(&self, external_id: &str) -> Result<usize>;
    fn delete_edge_row(&self, edge_key: &str) -> Result<usize>;
    fn node_row(&self, external_id: &str) -> Result<Option<ConvexNodeRow>>;
    fn node_rows(&self) -> Result<Vec<ConvexNodeRow>>;
    fn edge_rows(&self) -> Result<Vec<ConvexEdgeRow>>;
    fn node_rows_by_kind(&self, kind: &str) -> Result<Vec<ConvexNodeRow>>;
    fn outgoing_edge_rows(
        &self,
        from_external_id: &str,
        kind: Option<&str>,
    ) -> Result<Vec<ConvexEdgeRow>>;
}

#[derive(Default)]
pub struct ConvexRowsGraphClient {
    nodes: RefCell<BTreeMap<String, ConvexNodeRow>>,
    edges: RefCell<BTreeMap<String, ConvexEdgeRow>>,
}

impl ConvexRowsGraphClient {
    pub fn from_rows(rows: ConvexProjectionRows) -> Self {
        Self {
            nodes: RefCell::new(
                rows.nodes
                    .into_iter()
                    .map(|row| (row.external_id.clone(), row))
                    .collect(),
            ),
            edges: RefCell::new(
                rows.edges
                    .into_iter()
                    .map(|row| (row.edge_key.clone(), row))
                    .collect(),
            ),
        }
    }

    pub fn to_rows(&self) -> ConvexProjectionRows {
        ConvexProjectionRows {
            nodes: self.nodes.borrow().values().cloned().collect(),
            edges: self.edges.borrow().values().cloned().collect(),
        }
    }
}

impl ConvexGraphClient for ConvexRowsGraphClient {
    fn upsert_node_row(&self, row: &ConvexNodeRow) -> Result<()> {
        self.nodes
            .borrow_mut()
            .insert(row.external_id.clone(), row.clone());
        Ok(())
    }

    fn upsert_edge_row(&self, row: &ConvexEdgeRow) -> Result<()> {
        self.edges
            .borrow_mut()
            .insert(row.edge_key.clone(), row.clone());
        Ok(())
    }

    fn delete_node_row(&self, external_id: &str) -> Result<usize> {
        let mut edges = self.edges.borrow_mut();
        let incident = edges
            .values()
            .filter(|row| row.from_external_id == external_id || row.to_external_id == external_id)
            .map(|row| row.edge_key.clone())
            .collect::<Vec<_>>();
        for edge_key in incident {
            edges.remove(&edge_key);
        }
        Ok(usize::from(
            self.nodes.borrow_mut().remove(external_id).is_some(),
        ))
    }

    fn delete_edge_row(&self, edge_key: &str) -> Result<usize> {
        Ok(usize::from(
            self.edges.borrow_mut().remove(edge_key).is_some(),
        ))
    }

    fn node_row(&self, external_id: &str) -> Result<Option<ConvexNodeRow>> {
        Ok(self.nodes.borrow().get(external_id).cloned())
    }

    fn node_rows(&self) -> Result<Vec<ConvexNodeRow>> {
        Ok(self.nodes.borrow().values().cloned().collect())
    }

    fn edge_rows(&self) -> Result<Vec<ConvexEdgeRow>> {
        Ok(self.edges.borrow().values().cloned().collect())
    }

    fn node_rows_by_kind(&self, kind: &str) -> Result<Vec<ConvexNodeRow>> {
        Ok(self
            .nodes
            .borrow()
            .values()
            .filter(|row| row.kind == kind)
            .cloned()
            .collect())
    }

    fn outgoing_edge_rows(
        &self,
        from_external_id: &str,
        kind: Option<&str>,
    ) -> Result<Vec<ConvexEdgeRow>> {
        Ok(self
            .edges
            .borrow()
            .values()
            .filter(|row| row.from_external_id == from_external_id)
            .filter(|row| kind.is_none_or(|kind| row.kind == kind))
            .cloned()
            .collect())
    }
}

pub struct ConvexGraphStore<C> {
    client: C,
}

impl<C> ConvexGraphStore<C> {
    pub fn new(client: C) -> Self {
        Self { client }
    }

    pub fn client(&self) -> &C {
        &self.client
    }

    pub fn into_inner(self) -> C {
        self.client
    }
}

impl<C: ConvexGraphClient> GraphStore for ConvexGraphStore<C> {
    fn upsert_node(&self, node: &GraphNode) -> Result<()> {
        self.client.upsert_node_row(&ConvexNodeRow::from(node))
    }

    fn upsert_edge(&self, edge: &GraphEdge) -> Result<()> {
        if self.client.node_row(&edge.from_id)?.is_none() {
            bail!(
                "convex graph edge {} -> {} ({}) references missing from node",
                edge.from_id,
                edge.to_id,
                edge.kind
            );
        }
        if self.client.node_row(&edge.to_id)?.is_none() {
            bail!(
                "convex graph edge {} -> {} ({}) references missing to node",
                edge.from_id,
                edge.to_id,
                edge.kind
            );
        }
        self.client.upsert_edge_row(&ConvexEdgeRow::from(edge))
    }

    fn delete_node(&self, id: &str) -> Result<usize> {
        let incident = self
            .client
            .edge_rows()?
            .into_iter()
            .filter(|row| row.from_external_id == id || row.to_external_id == id)
            .map(|row| row.edge_key)
            .collect::<Vec<_>>();
        for edge_key in incident {
            self.client.delete_edge_row(&edge_key)?;
        }
        self.client.delete_node_row(id)
    }

    fn delete_edge(&self, from_id: &str, to_id: &str, kind: &str) -> Result<usize> {
        self.client
            .delete_edge_row(&ConvexEdgeRow::stable_key(from_id, to_id, kind))
    }

    fn node(&self, id: &str) -> Result<Option<GraphNode>> {
        Ok(self.client.node_row(id)?.map(GraphNode::from))
    }

    fn all_nodes(&self) -> Result<Vec<GraphNode>> {
        let mut nodes: Vec<GraphNode> = self
            .client
            .node_rows()?
            .into_iter()
            .map(GraphNode::from)
            .collect();
        nodes.sort_by(|left, right| left.id.cmp(&right.id));
        Ok(nodes)
    }

    fn all_edges(&self) -> Result<Vec<GraphEdge>> {
        let mut edges: Vec<GraphEdge> = self
            .client
            .edge_rows()?
            .into_iter()
            .map(GraphEdge::from)
            .collect();
        edges.sort_by(|left, right| {
            left.from_id
                .cmp(&right.from_id)
                .then(left.kind.cmp(&right.kind))
                .then(left.to_id.cmp(&right.to_id))
        });
        Ok(edges)
    }

    fn graph_counts(&self) -> Result<(usize, usize)> {
        Ok((
            self.client.node_rows()?.len(),
            self.client.edge_rows()?.len(),
        ))
    }

    fn nodes_by_kind(&self, kind: &str) -> Result<Vec<GraphNode>> {
        let mut nodes: Vec<GraphNode> = self
            .client
            .node_rows_by_kind(kind)?
            .into_iter()
            .map(GraphNode::from)
            .collect();
        nodes.sort_by(|left, right| left.id.cmp(&right.id));
        Ok(nodes)
    }

    fn outgoing_edges(&self, from_id: &str, kind: Option<&str>) -> Result<Vec<GraphEdge>> {
        let mut edges: Vec<GraphEdge> = self
            .client
            .outgoing_edge_rows(from_id, kind)?
            .into_iter()
            .map(GraphEdge::from)
            .collect();
        edges.sort_by(|left, right| {
            left.to_id
                .cmp(&right.to_id)
                .then(left.kind.cmp(&right.kind))
        });
        Ok(edges)
    }

    fn shortest_path(
        &self,
        from_id: &str,
        to_id: &str,
        kind: Option<&str>,
    ) -> Result<Option<GraphPath>> {
        shortest_path_using_outgoing(from_id, to_id, kind, None, |current, kind| {
            self.outgoing_edges(current, kind)
        })
    }
}

pub fn shortest_path_using_outgoing(
    from_id: &str,
    to_id: &str,
    kind: Option<&str>,
    max_hops: Option<usize>,
    mut outgoing_edges: impl FnMut(&str, Option<&str>) -> Result<Vec<GraphEdge>>,
) -> Result<Option<GraphPath>> {
    if from_id == to_id {
        return Ok(Some(GraphPath {
            nodes: vec![from_id.to_string()],
            hops: 0,
        }));
    }

    let mut queue = VecDeque::new();
    let mut parent = BTreeMap::<String, (usize, String)>::new();
    parent.insert(from_id.to_string(), (0, String::new()));
    queue.push_back(from_id.to_string());

    while let Some(current) = queue.pop_front() {
        let current_depth = parent.get(&current).map(|(d, _)| *d).unwrap_or(0);
        if max_hops.is_some_and(|max_hops| current_depth >= max_hops) {
            continue;
        }
        for edge in outgoing_edges(&current, kind)? {
            if parent.contains_key(&edge.to_id) {
                continue;
            }
            parent.insert(edge.to_id.clone(), (current_depth + 1, current.clone()));
            if edge.to_id == to_id {
                let mut nodes = vec![to_id.to_string()];
                let mut cursor = to_id;
                while let Some((_, previous)) = parent.get(cursor) {
                    if previous.is_empty() {
                        break;
                    }
                    nodes.push(previous.clone());
                    cursor = previous;
                }
                nodes.reverse();
                let hops = nodes.len().saturating_sub(1);
                return Ok(Some(GraphPath { nodes, hops }));
            }
            queue.push_back(edge.to_id);
        }
    }

    Ok(None)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_provenance() -> GraphProvenance {
        GraphProvenance::new("fixture", "src/lib.rs:1").with_content_hash("hash-1")
    }

    fn sample_projection() -> GraphProjection {
        let source = sample_provenance();
        GraphProjection {
            nodes: vec![
                GraphNode::new("doc:livekit", "document", "LiveKit guide")
                    .with_property("domain", "livekit")
                    .with_provenance(source.clone())
                    .with_freshness(GraphFreshness::content_hash("node-hash")),
                GraphNode::new("topic:rooms", "topic", "Rooms"),
                GraphNode::new("topic:egress", "topic", "Egress"),
            ],
            edges: vec![
                GraphEdge::new("doc:livekit", "topic:rooms", "mentions")
                    .with_property("confidence", "0.91")
                    .with_provenance(source.clone())
                    .with_freshness(GraphFreshness::content_hash("edge-hash")),
                GraphEdge::new("topic:rooms", "topic:egress", "related_to").with_provenance(source),
            ],
        }
    }

    fn assert_projection_store_contract(store: &impl GraphStore) {
        let projection = sample_projection();
        projection.upsert_into(store).unwrap();

        assert_eq!(
            store.node("doc:livekit").unwrap(),
            projection
                .nodes
                .iter()
                .find(|node| node.id == "doc:livekit")
                .cloned()
        );
        assert_eq!(
            store.nodes_by_kind("topic").unwrap(),
            vec![
                GraphNode::new("topic:egress", "topic", "Egress"),
                GraphNode::new("topic:rooms", "topic", "Rooms"),
            ]
        );

        let mentions = store
            .outgoing_edges("doc:livekit", Some("mentions"))
            .unwrap();
        assert_eq!(mentions.len(), 1);
        assert_eq!(mentions[0].to_id, "topic:rooms");
        assert_eq!(
            mentions[0].properties.get("confidence"),
            Some(&"0.91".into())
        );

        let path = store
            .shortest_path("doc:livekit", "topic:egress", None)
            .unwrap()
            .unwrap();
        assert_eq!(
            path.nodes,
            vec!["doc:livekit", "topic:rooms", "topic:egress"]
        );
    }

    #[derive(Default)]
    struct MemoryConvexGraphClient {
        nodes: RefCell<BTreeMap<String, ConvexNodeRow>>,
        edges: RefCell<BTreeMap<String, ConvexEdgeRow>>,
    }

    impl ConvexGraphClient for MemoryConvexGraphClient {
        fn upsert_node_row(&self, row: &ConvexNodeRow) -> Result<()> {
            self.nodes
                .borrow_mut()
                .insert(row.external_id.clone(), row.clone());
            Ok(())
        }

        fn upsert_edge_row(&self, row: &ConvexEdgeRow) -> Result<()> {
            self.edges
                .borrow_mut()
                .insert(row.edge_key.clone(), row.clone());
            Ok(())
        }

        fn delete_node_row(&self, external_id: &str) -> Result<usize> {
            Ok(usize::from(
                self.nodes.borrow_mut().remove(external_id).is_some(),
            ))
        }

        fn delete_edge_row(&self, edge_key: &str) -> Result<usize> {
            Ok(usize::from(
                self.edges.borrow_mut().remove(edge_key).is_some(),
            ))
        }

        fn node_row(&self, external_id: &str) -> Result<Option<ConvexNodeRow>> {
            Ok(self.nodes.borrow().get(external_id).cloned())
        }

        fn node_rows(&self) -> Result<Vec<ConvexNodeRow>> {
            Ok(self.nodes.borrow().values().cloned().collect())
        }

        fn edge_rows(&self) -> Result<Vec<ConvexEdgeRow>> {
            Ok(self.edges.borrow().values().cloned().collect())
        }

        fn node_rows_by_kind(&self, kind: &str) -> Result<Vec<ConvexNodeRow>> {
            Ok(self
                .nodes
                .borrow()
                .values()
                .filter(|row| row.kind == kind)
                .cloned()
                .collect())
        }

        fn outgoing_edge_rows(
            &self,
            from_external_id: &str,
            kind: Option<&str>,
        ) -> Result<Vec<ConvexEdgeRow>> {
            Ok(self
                .edges
                .borrow()
                .values()
                .filter(|row| row.from_external_id == from_external_id)
                .filter(|row| kind.is_none_or(|kind| row.kind == kind))
                .cloned()
                .collect())
        }
    }

    #[test]
    fn graph_projection_round_trips_through_backend_agnostic_store_contract() {
        let convex = ConvexGraphStore::new(MemoryConvexGraphClient::default());
        assert_projection_store_contract(&convex);

        let client = convex.client();
        assert_eq!(client.nodes.borrow().len(), 3);
        assert_eq!(client.edges.borrow().len(), 2);
        assert!(
            client.nodes.borrow().contains_key("doc:livekit"),
            "Convex rows keep GraphNode.id as the externalId upsert key"
        );
    }

    #[test]
    fn graph_store_contract_covers_crud_neighborhood_and_ordering() {
        fn assert_crud_contract(store: &impl GraphStore) {
            let projection = sample_projection();
            projection.upsert_into(store).unwrap();

            let neighborhood = store.neighborhood("doc:livekit", 2, None).unwrap().unwrap();
            assert_eq!(
                neighborhood
                    .nodes
                    .iter()
                    .map(|node| node.id.as_str())
                    .collect::<Vec<_>>(),
                vec!["doc:livekit", "topic:egress", "topic:rooms"]
            );
            assert_eq!(
                neighborhood
                    .edges
                    .iter()
                    .map(|edge| (
                        edge.from_id.as_str(),
                        edge.kind.as_str(),
                        edge.to_id.as_str()
                    ))
                    .collect::<Vec<_>>(),
                vec![
                    ("doc:livekit", "mentions", "topic:rooms"),
                    ("topic:rooms", "related_to", "topic:egress"),
                ]
            );

            assert_eq!(
                store
                    .delete_edge("topic:rooms", "topic:egress", "related_to")
                    .unwrap(),
                1
            );
            assert!(
                store
                    .shortest_path("doc:livekit", "topic:egress", None)
                    .unwrap()
                    .is_none()
            );
            assert_eq!(store.delete_node("topic:rooms").unwrap(), 1);
            assert!(store.node("topic:rooms").unwrap().is_none());
            assert!(
                store
                    .outgoing_edges("doc:livekit", None)
                    .unwrap()
                    .is_empty()
            );
        }

        assert_crud_contract(&ConvexGraphStore::new(ConvexRowsGraphClient::default()));
    }

    #[test]
    fn convex_projection_rows_keep_stable_ids_and_edge_keys() {
        let projection = sample_projection();
        let rows = projection.to_convex_rows();

        let doc_row = rows
            .nodes
            .iter()
            .find(|row| row.external_id == "doc:livekit")
            .unwrap();
        assert_eq!(doc_row.kind, "document");
        assert_eq!(doc_row.properties.get("domain"), Some(&"livekit".into()));

        let mentions = rows
            .edges
            .iter()
            .find(|row| row.kind == "mentions")
            .unwrap();
        assert_eq!(mentions.from_external_id, "doc:livekit");
        assert_eq!(mentions.to_external_id, "topic:rooms");
        assert_eq!(
            mentions.edge_key,
            ConvexEdgeRow::stable_key("doc:livekit", "topic:rooms", "mentions")
        );
        assert!(mentions.edge_key.starts_with("edge:"));
    }

    #[test]
    fn terse_graph_node_strips_provenance_and_freshness() {
        let node = GraphNode::new("doc:livekit", "document", "LiveKit guide")
            .with_property("domain", "livekit")
            .with_provenance(GraphProvenance::new("fixture", "src/lib.rs:1"))
            .with_freshness(GraphFreshness::content_hash("hash-1"));
        let terse = TerseGraphNode::from(&node);
        assert_eq!(terse.id, "doc:livekit");
        assert_eq!(terse.kind, "document");
        assert_eq!(terse.label, "LiveKit guide");
        assert_eq!(terse.properties.get("domain"), Some(&"livekit".to_string()));
        let json = serde_json::to_value(&terse).unwrap();
        assert!(json.get("provenance").is_none());
        assert!(json.get("freshness").is_none());
        let full_json = serde_json::to_string(&node).unwrap();
        let terse_json = serde_json::to_string(&terse).unwrap();
        assert!(
            terse_json.len() < full_json.len(),
            "terse ({}) should be shorter than full ({})",
            terse_json.len(),
            full_json.len()
        );
    }

    #[test]
    fn terse_graph_edge_strips_provenance_and_freshness() {
        let edge = GraphEdge::new("a", "b", "calls")
            .with_property("line", "10")
            .with_provenance(GraphProvenance::new("fixture", "src/lib.rs:5"))
            .with_freshness(GraphFreshness::content_hash("edge-hash"));
        let terse = TerseGraphEdge::from(&edge);
        assert_eq!(terse.from_id, "a");
        assert_eq!(terse.to_id, "b");
        assert_eq!(terse.kind, "calls");
        assert_eq!(terse.properties.get("line"), Some(&"10".to_string()));
        let json = serde_json::to_value(&terse).unwrap();
        assert!(json.get("provenance").is_none());
        assert!(json.get("freshness").is_none());
    }

    #[test]
    fn terse_graph_subgraph_rounds_trip() {
        let subgraph = GraphSubgraph {
            nodes: vec![
                GraphNode::new("a", "fn", "alpha")
                    .with_provenance(GraphProvenance::new("src", "a.rs")),
                GraphNode::new("b", "fn", "beta"),
            ],
            edges: vec![GraphEdge::new("a", "b", "calls")
                .with_freshness(GraphFreshness::content_hash("h"))],
        }
        .sorted();
        let terse = TerseGraphSubgraph::from(subgraph);
        assert_eq!(terse.nodes.len(), 2);
        assert_eq!(terse.edges.len(), 1);
        assert_eq!(terse.edges[0].from_id, "a");
    }

    #[test]
    fn convex_store_rejects_edges_when_projection_nodes_are_missing() {
        let store = ConvexGraphStore::new(MemoryConvexGraphClient::default());
        store
            .upsert_node(&GraphNode::new("doc:livekit", "document", "LiveKit guide"))
            .unwrap();

        let err = store
            .upsert_edge(&GraphEdge::new("doc:livekit", "topic:rooms", "mentions"))
            .unwrap_err();
        assert!(
            err.to_string().contains("references missing to node"),
            "{err}"
        );
    }
}
