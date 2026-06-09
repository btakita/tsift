use anyhow::Result;
use lazily::{Context as LazyContext, SlotHandle};
use std::collections::{BTreeMap, BTreeSet, BinaryHeap, VecDeque};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::types::*;

pub(crate) fn graph_node_matches_filters(
    node: &GraphNode,
    filters: &[GraphPropertyFilter],
) -> bool {
    filters
        .iter()
        .all(|filter| node.properties.get(&filter.key) == Some(&filter.value))
}

#[derive(Debug, Clone, Eq, PartialEq)]
pub(crate) struct ScoredQueueEntry {
    pub id: String,
    pub depth: usize,
    pub score: i64,
}

impl Ord for ScoredQueueEntry {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.score
            .cmp(&other.score)
            .then_with(|| other.depth.cmp(&self.depth))
            .then_with(|| self.id.cmp(&other.id))
    }
}

impl PartialOrd for ScoredQueueEntry {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

pub(crate) fn compute_neighborhood_score(
    strategy: &NeighborhoodScoring,
    depth: usize,
    edge_kind: &str,
    _node: &GraphNode,
    degree_map: &BTreeMap<String, usize>,
) -> i64 {
    match strategy {
        NeighborhoodScoring::BreadthFirst => {
            (120i64.saturating_sub((depth as i64).saturating_mul(18))).max(0)
        }
        NeighborhoodScoring::EdgeKindWeighted => {
            let depth_score = (120i64.saturating_sub((depth as i64).saturating_mul(18))).max(0);
            let kind_score = edge_kind_weighted_score(edge_kind);
            depth_score.saturating_add(kind_score)
        }
        NeighborhoodScoring::DegreeWeighted => {
            let depth_score = (120i64.saturating_sub((depth as i64).saturating_mul(18))).max(0);
            let degree = degree_map.values().copied().max().unwrap_or(1) as i64;
            let degree_bonus = if degree <= 3 {
                20
            } else if degree <= 10 {
                10
            } else {
                0
            };
            depth_score.saturating_add(degree_bonus)
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct NeighborhoodScoreContext {
    pub now_unix: i64,
}

impl NeighborhoodScoreContext {
    pub(crate) fn from_options(options: &RankedNeighborhoodOptions) -> Self {
        Self {
            now_unix: options.observed_at_now_unix.unwrap_or_else(unix_now),
        }
    }
}

pub(crate) fn compute_ranked_neighborhood_score(
    options: &RankedNeighborhoodOptions,
    context: NeighborhoodScoreContext,
    depth: usize,
    edge_kind: &str,
    node: &GraphNode,
    degree_map: &BTreeMap<String, usize>,
) -> i64 {
    compute_neighborhood_score(&options.scoring, depth, edge_kind, node, degree_map)
        .saturating_add(observed_at_decay_bonus(
            node,
            context.now_unix,
            options.observed_at_half_life_secs,
            options.observed_at_weight,
        ))
        .saturating_add(memory_node_signal_bonus(node, options.memory_node_boost))
}

pub(crate) fn graph_node_observed_at_unix(node: &GraphNode) -> Option<i64> {
    node.freshness
        .as_ref()
        .and_then(|freshness| freshness.observed_at_unix)
        .or_else(|| graph_node_i64_property(node, "observed_at_unix"))
        .or_else(|| graph_node_i64_property(node, "max_observed_at_unix"))
}

pub(crate) fn observed_at_decay_bonus(
    node: &GraphNode,
    now_unix: i64,
    half_life_secs: i64,
    weight: i64,
) -> i64 {
    if weight <= 0 {
        return 0;
    }
    let Some(observed_at_unix) = graph_node_observed_at_unix(node) else {
        return 0;
    };
    let half_life_secs = half_life_secs.max(1);
    let age_secs = now_unix.saturating_sub(observed_at_unix).max(0);
    match age_secs / half_life_secs {
        0 => weight,
        1 => weight / 2,
        2 | 3 => weight / 4,
        _ => 0,
    }
}

pub(crate) fn memory_node_signal_bonus(node: &GraphNode, configured_boost: i64) -> i64 {
    if configured_boost <= 0 {
        return 0;
    }
    let provider_memory = node
        .properties
        .get("provider")
        .is_some_and(|provider| provider == "tsift-memory");
    let authored_kind = matches!(node.kind.as_str(), "finding" | "decision" | "note");
    let memory_kind = node.kind.starts_with("memory_") || provider_memory || authored_kind;
    if !memory_kind || node.kind == "memory_projection" {
        return 0;
    }

    let base = match node.kind.as_str() {
        "finding" | "decision" | "memory_event" => configured_boost,
        "note" | "memory_session" => configured_boost / 2,
        "source_handle" | "semantic_concept" | "semantic_vector_handle" if provider_memory => {
            configured_boost / 2
        }
        _ if provider_memory => configured_boost / 2,
        _ => configured_boost,
    };

    base.saturating_add(confidence_bonus(node, configured_boost))
}

fn confidence_bonus(node: &GraphNode, configured_boost: i64) -> i64 {
    node.properties
        .get("confidence")
        .and_then(|value| value.parse::<f64>().ok())
        .map(|confidence| (configured_boost as f64 * confidence.clamp(0.0, 1.0)).round() as i64)
        .unwrap_or(0)
}

fn graph_node_i64_property(node: &GraphNode, key: &str) -> Option<i64> {
    node.properties.get(key)?.parse().ok()
}

fn unix_now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs() as i64)
        .unwrap_or_default()
}

#[derive(Clone)]
pub(crate) struct NeighborhoodLayerState {
    pub nodes: BTreeMap<String, GraphNode>,
    pub edges: BTreeMap<(String, String, String), GraphEdge>,
    pub frontier: Vec<String>,
}

#[derive(Clone)]
pub(crate) struct NeighborhoodFetchedLayer {
    pub edges: Vec<GraphEdge>,
    pub nodes: Vec<GraphNode>,
}

#[derive(Clone)]
pub(crate) struct RankedNeighborhoodLayerState {
    pub nodes: BTreeMap<String, GraphNode>,
    pub edges: BTreeMap<(String, String, String), GraphEdge>,
    pub queue: BinaryHeap<ScoredQueueEntry>,
    pub seen: BTreeSet<String>,
    pub pruned_count: usize,
    pub total_discovered: usize,
    pub degree_map: BTreeMap<String, usize>,
}

#[derive(Clone)]
pub(crate) struct RankedNeighborhoodFetchedExpansion {
    pub edges: Vec<GraphEdge>,
    pub neighbor_nodes: BTreeMap<String, GraphNode>,
}

pub(crate) fn graph_cache_error(message: String) -> anyhow::Error {
    anyhow::anyhow!("{message}")
}

pub(crate) fn fetch_neighborhood_layer<S: GraphStore + ?Sized>(
    store: &S,
    frontier: &[String],
    known_nodes: &BTreeMap<String, GraphNode>,
    kind: Option<&str>,
) -> Result<NeighborhoodFetchedLayer> {
    let mut edges = Vec::new();
    let mut missing_ids = Vec::new();
    for current in frontier {
        let outgoing = store.outgoing_edges(current, kind)?;
        for edge in outgoing {
            if !known_nodes.contains_key(&edge.to_id) {
                missing_ids.push(edge.to_id.clone());
            }
            edges.push(edge);
        }
    }
    missing_ids.sort();
    missing_ids.dedup();
    let mut nodes = Vec::new();
    for id in missing_ids {
        if !known_nodes.contains_key(&id)
            && let Some(node) = store.node(&id)?
        {
            nodes.push(node);
        }
    }
    Ok(NeighborhoodFetchedLayer { edges, nodes })
}

pub(crate) fn fetch_ranked_neighborhood_expansion<S: GraphStore + ?Sized>(
    store: &S,
    entry: &ScoredQueueEntry,
    state: &RankedNeighborhoodLayerState,
    kind: Option<&str>,
) -> Result<RankedNeighborhoodFetchedExpansion> {
    let edges = store.outgoing_edges(&entry.id, kind)?;
    let mut neighbor_nodes = BTreeMap::new();
    for edge in &edges {
        if state.seen.contains(&edge.to_id) {
            continue;
        }
        if let Some(node) = store.node(&edge.to_id)? {
            neighbor_nodes.insert(edge.to_id.clone(), node);
        }
    }
    Ok(RankedNeighborhoodFetchedExpansion {
        edges,
        neighbor_nodes,
    })
}

pub(crate) fn edge_kind_weighted_score(edge_kind: &str) -> i64 {
    match edge_kind {
        "semantic_relation" => 34,
        "mentions_entity" | "mentions_concept" | "tagged_entity" | "tagged_concept"
        | "related_concept" => 28,
        "mentions" => 22,
        "calls" => 20,
        "requests_context" | "scopes_context" | "scopes_source" | "explains_result" => 18,
        "defines" | "contains" | "belongs_to" => 12,
        kind if kind.contains("community") => 20,
        kind if kind.contains("semantic")
            || kind.contains("concept")
            || kind.contains("entity") =>
        {
            24
        }
        _ => 8,
    }
}

pub(crate) fn graph_edge_matches_filters(
    edge: &GraphEdge,
    filters: &[GraphPropertyFilter],
) -> bool {
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

pub fn parse_graph_semantic_vector_property(value: &str) -> Option<Vec<f64>> {
    let parsed = value
        .split(',')
        .map(|part| part.trim().parse::<f64>())
        .collect::<std::result::Result<Vec<_>, _>>()
        .ok()?;
    (!parsed.is_empty() && parsed.iter().all(|value| value.is_finite())).then_some(parsed)
}

pub fn graph_semantic_cosine(left: &[f64], right: &[f64]) -> f64 {
    if left.len() != right.len() || left.is_empty() {
        return 0.0;
    }
    left.iter()
        .zip(right)
        .map(|(left, right)| left * right)
        .sum::<f64>()
}

pub fn graph_semantic_seeded_edge_other_id<'a>(
    edge: &'a GraphEdge,
    current_id: &str,
) -> Option<&'a str> {
    if edge.from_id == current_id {
        Some(edge.to_id.as_str())
    } else if edge.to_id == current_id {
        Some(edge.from_id.as_str())
    } else {
        None
    }
}

pub fn graph_semantic_seeded_edge_score(edge: &GraphEdge, current_id: &str) -> i64 {
    let mut score = edge_kind_weighted_score(&edge.kind).saturating_mul(10);
    score += if edge.from_id == current_id { 8 } else { 4 };
    score += match edge.kind.as_str() {
        "mentions_concept" | "mentions_entity" | "tagged_concept" | "tagged_entity"
        | "related_concept" => 30,
        "semantic_relation" => 28,
        "calls" => 24,
        "mentions" => 22,
        "requests_context" | "scopes_context" | "scopes_source" | "explains_result" => 18,
        "defines" | "contains" | "belongs_to" => 12,
        _ => 0,
    };
    score
}

pub trait GraphStore {
    fn upsert_node(&self, node: &GraphNode) -> Result<()>;
    fn upsert_edge(&self, edge: &GraphEdge) -> Result<()>;
    fn delete_node(&self, id: &str) -> Result<usize>;
    fn delete_edge(&self, from_id: &str, to_id: &str, kind: &str) -> Result<usize>;
    fn node(&self, id: &str) -> Result<Option<GraphNode>>;
    fn nodes_by_ids(&self, ids: &[String]) -> Result<Vec<GraphNode>> {
        let mut nodes = Vec::new();
        let mut seen = BTreeSet::new();
        for id in ids {
            if !seen.insert(id.clone()) {
                continue;
            }
            if let Some(node) = self.node(id)? {
                nodes.push(node);
            }
        }
        nodes.sort_by(|left, right| left.id.cmp(&right.id));
        Ok(nodes)
    }
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
    fn semantic_seeded_expansion_edges(
        &self,
        current_id: &str,
        options: &SemanticSeededNeighborhoodOptions,
    ) -> Result<SemanticSeededNeighborhoodExpansion> {
        let mut expansion_edges_by_key = BTreeMap::<String, GraphEdge>::new();
        for edge in self.outgoing_edges(current_id, None)? {
            expansion_edges_by_key
                .entry(graph_edge_id(&edge))
                .or_insert(edge);
        }
        for edge in self.incident_edges(current_id, None)? {
            expansion_edges_by_key
                .entry(graph_edge_id(&edge))
                .or_insert(edge);
        }
        let mut edges = expansion_edges_by_key.into_values().collect::<Vec<_>>();
        edges.sort_by(|left, right| {
            graph_semantic_seeded_edge_score(right, current_id)
                .cmp(&graph_semantic_seeded_edge_score(left, current_id))
                .then_with(|| graph_edge_id(left).cmp(&graph_edge_id(right)))
        });
        let mut skipped_by_edge_cap = 0usize;
        if options.edge_scan_cap > 0 && edges.len() > options.edge_scan_cap {
            skipped_by_edge_cap = edges.len() - options.edge_scan_cap;
            edges.truncate(options.edge_scan_cap);
        }
        Ok(SemanticSeededNeighborhoodExpansion {
            edges,
            skipped_by_edge_cap,
        })
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
        let ctx = LazyContext::new();
        let initial = NeighborhoodLayerState {
            nodes: BTreeMap::from([(center_id.to_string(), center)]),
            edges: BTreeMap::new(),
            frontier: vec![center_id.to_string()],
        };
        let mut layer: SlotHandle<std::result::Result<NeighborhoodLayerState, String>> =
            ctx.slot(move |_| Ok(initial.clone()));
        let mut state = ctx.get(&layer).map_err(graph_cache_error)?;

        for _ in 0..depth {
            if state.frontier.is_empty() {
                break;
            }
            let fetched = fetch_neighborhood_layer(self, &state.frontier, &state.nodes, kind)?;
            let previous = layer;
            layer = ctx.slot(move |ctx| {
                let mut state = ctx.get(&previous)?;
                for edge in &fetched.edges {
                    let edge_key = (edge.from_id.clone(), edge.kind.clone(), edge.to_id.clone());
                    state.edges.entry(edge_key).or_insert_with(|| edge.clone());
                }
                state.frontier.clear();
                for node in &fetched.nodes {
                    if !state.nodes.contains_key(&node.id) {
                        state.frontier.push(node.id.clone());
                        state.nodes.insert(node.id.clone(), node.clone());
                    }
                }
                Ok(state)
            });
            state = ctx.get(&layer).map_err(graph_cache_error)?;
        }

        Ok(Some(
            GraphSubgraph {
                nodes: state.nodes.into_values().collect(),
                edges: state.edges.into_values().collect(),
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
    fn semantic_seeded_neighborhood(
        &self,
        seed_ids: &[String],
        options: &SemanticSeededNeighborhoodOptions,
    ) -> Result<SemanticSeededNeighborhoodResult> {
        let seed_rank = seed_ids
            .iter()
            .enumerate()
            .map(|(idx, seed)| (seed.clone(), idx))
            .collect::<BTreeMap<_, _>>();
        let seed_nodes_by_id = self
            .nodes_by_ids(seed_ids)?
            .into_iter()
            .map(|node| (node.id.clone(), node))
            .collect::<BTreeMap<_, _>>();
        let mut nodes = BTreeMap::<String, GraphNode>::new();
        let mut edges = BTreeMap::<String, GraphEdge>::new();
        let mut node_score_by_id = BTreeMap::<String, i64>::new();
        let mut queue = VecDeque::<(String, usize)>::new();
        let mut seen_at_depth = BTreeMap::<String, usize>::new();
        let mut missing_seed_ids = Vec::new();
        let mut skipped_by_edge_cap = 0usize;
        let mut skipped_by_node_cap = 0usize;

        for (idx, seed_id) in seed_ids.iter().enumerate() {
            if let Some(node) = seed_nodes_by_id.get(seed_id) {
                nodes.entry(seed_id.clone()).or_insert_with(|| node.clone());
                node_score_by_id
                    .entry(seed_id.clone())
                    .or_insert(1_000_000i64.saturating_sub(idx as i64));
                if !seen_at_depth.contains_key(seed_id) {
                    queue.push_back((seed_id.clone(), 0));
                    seen_at_depth.insert(seed_id.clone(), 0);
                }
            } else {
                missing_seed_ids.push(seed_id.clone());
            }
        }

        while let Some((current_id, current_depth)) = queue.pop_front() {
            if current_depth >= options.depth {
                continue;
            }

            let expansion = self.semantic_seeded_expansion_edges(&current_id, options)?;
            skipped_by_edge_cap = skipped_by_edge_cap.saturating_add(expansion.skipped_by_edge_cap);
            let mut candidates = Vec::new();
            let mut missing_candidate_ids = Vec::new();
            for edge in expansion.edges {
                let Some(other_id) = graph_semantic_seeded_edge_other_id(&edge, &current_id) else {
                    continue;
                };
                let other_id = other_id.to_string();
                let edge_score = graph_semantic_seeded_edge_score(&edge, &current_id)
                    .saturating_add(
                        (options.depth.saturating_sub(current_depth) as i64).saturating_mul(5),
                    );
                if !nodes.contains_key(&other_id) {
                    missing_candidate_ids.push(other_id.clone());
                }
                candidates.push((edge, other_id, edge_score));
            }

            missing_candidate_ids.sort();
            missing_candidate_ids.dedup();
            let fetched_nodes_by_id = self
                .nodes_by_ids(&missing_candidate_ids)?
                .into_iter()
                .map(|node| (node.id.clone(), node))
                .collect::<BTreeMap<_, _>>();

            for (edge, other_id, edge_score) in candidates {
                let other_known = nodes.contains_key(&other_id);
                if !other_known && nodes.len() >= options.node_discovery_cap {
                    skipped_by_node_cap = skipped_by_node_cap.saturating_add(1);
                    continue;
                }
                node_score_by_id
                    .entry(other_id.clone())
                    .and_modify(|score| *score = (*score).max(edge_score))
                    .or_insert(edge_score);
                let edge_key = graph_edge_id(&edge);
                edges.entry(edge_key).or_insert(edge);
                if !other_known && let Some(node) = fetched_nodes_by_id.get(&other_id) {
                    nodes.insert(other_id.clone(), node.clone());
                }
                if !nodes.contains_key(&other_id) {
                    continue;
                }
                let next_depth = current_depth + 1;
                let should_queue = seen_at_depth
                    .get(&other_id)
                    .is_none_or(|seen_depth| next_depth < *seen_depth);
                if should_queue {
                    seen_at_depth.insert(other_id.clone(), next_depth);
                    queue.push_back((other_id, next_depth));
                }
            }
        }

        let mut nodes = nodes.into_values().collect::<Vec<_>>();
        nodes.sort_by(|left, right| {
            seed_rank
                .get(&left.id)
                .copied()
                .unwrap_or(usize::MAX)
                .cmp(&seed_rank.get(&right.id).copied().unwrap_or(usize::MAX))
                .then_with(|| {
                    node_score_by_id
                        .get(&right.id)
                        .copied()
                        .unwrap_or_default()
                        .cmp(&node_score_by_id.get(&left.id).copied().unwrap_or_default())
                })
                .then(left.id.cmp(&right.id))
        });

        let total_discovered = nodes.len();
        let truncated = options.limit > 0 && nodes.len() > options.limit;
        if truncated {
            nodes.truncate(options.limit);
        }

        let node_ids = nodes
            .iter()
            .map(|node| node.id.as_str())
            .collect::<BTreeSet<_>>();
        let mut edges = edges
            .into_values()
            .filter(|edge| {
                node_ids.contains(edge.from_id.as_str()) && node_ids.contains(edge.to_id.as_str())
            })
            .collect::<Vec<_>>();
        edges.sort_by_key(graph_edge_id);

        Ok(SemanticSeededNeighborhoodResult {
            nodes,
            edges,
            skipped_by_edge_cap,
            skipped_by_node_cap,
            missing_seed_ids,
            total_discovered,
            truncated,
        })
    }
    fn ranked_neighborhood(
        &self,
        center_id: &str,
        options: &RankedNeighborhoodOptions,
    ) -> Result<Option<RankedNeighborhoodResult>> {
        let Some(center) = self.node(center_id)? else {
            return Ok(None);
        };
        let ctx = LazyContext::new();
        let initial = RankedNeighborhoodLayerState {
            nodes: BTreeMap::from([(center_id.to_string(), center)]),
            edges: BTreeMap::new(),
            queue: BinaryHeap::from([ScoredQueueEntry {
                id: center_id.to_string(),
                depth: 0usize,
                score: i64::MAX,
            }]),
            seen: BTreeSet::from([center_id.to_string()]),
            pruned_count: 0,
            total_discovered: 1,
            degree_map: BTreeMap::new(),
        };
        let mut layer: SlotHandle<std::result::Result<RankedNeighborhoodLayerState, String>> =
            ctx.slot(move |_| Ok(initial.clone()));
        let mut state = ctx.get(&layer).map_err(graph_cache_error)?;
        let options = options.clone();
        let score_context = NeighborhoodScoreContext::from_options(&options);

        while let Some(entry) = state.queue.peek().cloned() {
            let fetched = if entry.depth < options.depth {
                Some(fetch_ranked_neighborhood_expansion(
                    self,
                    &entry,
                    &state,
                    options.edge_kind.as_deref(),
                )?)
            } else {
                None
            };
            let previous = layer;
            let options = options.clone();
            layer = ctx.slot(move |ctx| {
                let mut state = ctx.get(&previous)?;
                let Some(entry) = state.queue.pop() else {
                    return Ok(state);
                };
                let Some(fetched) = &fetched else {
                    return Ok(state);
                };
                let mut candidates = Vec::new();
                for edge in &fetched.edges {
                    let edge_key = (edge.from_id.clone(), edge.kind.clone(), edge.to_id.clone());
                    state.edges.entry(edge_key).or_insert_with(|| edge.clone());
                    *state.degree_map.entry(edge.from_id.clone()).or_default() += 1;
                    *state.degree_map.entry(edge.to_id.clone()).or_default() += 1;
                    if state.seen.contains(&edge.to_id) {
                        continue;
                    }
                    let Some(neighbor) = fetched.neighbor_nodes.get(&edge.to_id) else {
                        continue;
                    };
                    let score = compute_ranked_neighborhood_score(
                        &options,
                        score_context,
                        entry.depth + 1,
                        &edge.kind,
                        neighbor,
                        &state.degree_map,
                    );
                    candidates.push((edge.to_id.clone(), neighbor.clone(), score));
                }
                candidates.sort_by(|a, b| b.2.cmp(&a.2).then_with(|| a.0.cmp(&b.0)));
                for (to_id, neighbor, score) in candidates {
                    if !state.seen.insert(to_id.clone()) {
                        continue;
                    }
                    state.total_discovered += 1;
                    if state.nodes.len() > options.max_nodes {
                        state.pruned_count += 1;
                        continue;
                    }
                    state.nodes.insert(to_id.clone(), neighbor);
                    state.queue.push(ScoredQueueEntry {
                        id: to_id,
                        depth: entry.depth + 1,
                        score,
                    });
                }
                Ok(state)
            });
            state = ctx.get(&layer).map_err(graph_cache_error)?;
        }

        let node_ids: BTreeSet<_> = state.nodes.keys().cloned().collect();
        state
            .edges
            .retain(|_, edge| node_ids.contains(&edge.from_id) && node_ids.contains(&edge.to_id));

        Ok(Some(RankedNeighborhoodResult {
            nodes: state.nodes.into_values().collect(),
            edges: state.edges.into_values().collect(),
            pruned_count: state.pruned_count,
            total_discovered: state.total_discovered,
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

    fn semantic_top_candidates(
        &self,
        query_vector: &[f64],
        kinds: &[&str],
        limit: usize,
    ) -> Result<Vec<GraphSemanticCandidate>> {
        graph_semantic_top_candidates_by_property_scan(self, query_vector, kinds, limit)
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

pub fn graph_semantic_top_candidates_by_property_scan<S: GraphStore + ?Sized>(
    store: &S,
    query_vector: &[f64],
    kinds: &[&str],
    limit: usize,
) -> Result<Vec<GraphSemanticCandidate>> {
    if query_vector.is_empty() || kinds.is_empty() {
        return Ok(Vec::new());
    }
    let mut seen_kinds = BTreeSet::new();
    let mut candidates = Vec::new();
    for kind in kinds {
        if !seen_kinds.insert(*kind) {
            continue;
        }
        for node in store.nodes_by_kind(kind)? {
            let Some(vector) = node
                .properties
                .get(GRAPH_SEMANTIC_VECTOR_PROPERTY_KEY)
                .and_then(|value| parse_graph_semantic_vector_property(value))
            else {
                continue;
            };
            if vector.len() != query_vector.len() {
                continue;
            }
            candidates.push(GraphSemanticCandidate {
                score: graph_semantic_cosine(query_vector, &vector),
                node,
            });
        }
    }
    candidates.sort_by(|left, right| {
        right
            .score
            .partial_cmp(&left.score)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| left.node.kind.cmp(&right.node.kind))
            .then_with(|| left.node.label.cmp(&right.node.label))
            .then_with(|| left.node.id.cmp(&right.node.id))
    });
    if limit > 0 && candidates.len() > limit {
        candidates.truncate(limit);
    }
    Ok(candidates)
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
