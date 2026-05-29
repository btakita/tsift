use std::collections::{BTreeMap, BTreeSet, VecDeque};

use serde::Serialize;
use tsift_core::{GraphEdge, GraphNode};

pub const COMMUNITY_MIN_HANDLE_COVERAGE_PCT: f64 = 95.0;
pub const COMMUNITY_MIN_DUPLICATE_NAME_PRECISION: f64 = 0.99;

#[derive(Clone, Debug, Serialize, PartialEq)]
pub struct RankedNeighbor {
    pub rank: usize,
    pub node_id: String,
    pub kind: String,
    pub label: String,
    pub score: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub depth: Option<usize>,
    pub edge_kinds: Vec<String>,
    pub community_co_membership: bool,
    pub semantic_relation: bool,
    pub source_handle_fresh: bool,
    pub duplicate_name_precision: f64,
    pub handle_coverage_pct: f64,
    pub reasons: Vec<String>,
}

#[derive(Clone, Debug, Serialize, PartialEq)]
pub struct NeighborhoodRankingGate {
    pub status: String,
    pub ranked_output_default: bool,
    pub default_order: String,
    pub default_change_gate: String,
    pub required_workloads: Vec<String>,
    pub required_metrics: Vec<String>,
    pub max_duration_regression_percent: f64,
    pub min_handle_coverage_pct: f64,
    pub min_duplicate_name_precision: f64,
    pub min_top_community_stability: f64,
    pub diagnostics: Vec<String>,
}

pub fn ranked_neighbors(
    center_id: &str,
    nodes: &[GraphNode],
    edges: &[GraphEdge],
) -> Vec<RankedNeighbor> {
    let node_by_id = nodes
        .iter()
        .map(|node| (node.id.clone(), node))
        .collect::<BTreeMap<_, _>>();
    let label_counts = nodes
        .iter()
        .fold(BTreeMap::<String, usize>::new(), |mut acc, node| {
            *acc.entry(node.label.clone()).or_default() += 1;
            acc
        });
    let mut outgoing = BTreeMap::<String, Vec<&GraphEdge>>::new();
    let mut edge_kinds_by_node = BTreeMap::<String, BTreeSet<String>>::new();
    let mut fresh_edge_by_node = BTreeSet::<String>::new();
    for edge in edges {
        outgoing.entry(edge.from_id.clone()).or_default().push(edge);
        for endpoint in [&edge.from_id, &edge.to_id] {
            if node_by_id.contains_key(endpoint) {
                edge_kinds_by_node
                    .entry(endpoint.clone())
                    .or_default()
                    .insert(edge.kind.clone());
                if edge.freshness.as_ref().is_some_and(|freshness| {
                    freshness.content_hash.is_some() || freshness.observed_at_unix.is_some()
                }) {
                    fresh_edge_by_node.insert(endpoint.clone());
                }
            }
        }
    }

    let depths = neighborhood_depths(center_id, &node_by_id, &outgoing);
    let page_handle_coverage_pct = page_handle_coverage_pct(nodes);
    let mut ranked = Vec::new();

    for node in nodes {
        if node.id == center_id {
            continue;
        }
        let edge_kinds = edge_kinds_by_node
            .get(&node.id)
            .map(|kinds| kinds.iter().cloned().collect::<Vec<_>>())
            .unwrap_or_default();
        let depth = depths.get(&node.id).copied();
        let community_co_membership = has_community_signal(node, &edge_kinds);
        let semantic_relation = has_semantic_signal(node, &edge_kinds);
        let source_handle_fresh = source_handle_is_fresh(node)
            || (node.kind == "source_handle" && fresh_edge_by_node.contains(&node.id));
        let duplicate_name_precision = duplicate_name_precision(node, &label_counts);
        let mut score = 0i64;
        let mut reasons = Vec::new();

        if let Some(depth) = depth {
            let depth_score = 120i64
                .saturating_sub((depth as i64).saturating_mul(18))
                .max(0);
            score += depth_score;
            reasons.push(format!("depth:{depth}+{depth_score}"));
        } else {
            reasons.push("depth:unknown".to_string());
        }

        for edge_kind in &edge_kinds {
            let edge_score = edge_kind_rank_score(edge_kind);
            score += edge_score;
            reasons.push(format!("edge_kind:{edge_kind}+{edge_score}"));
        }
        if community_co_membership {
            score += 18;
            reasons.push("community_co_membership+18".to_string());
        }
        if semantic_relation {
            score += 24;
            reasons.push("semantic_relation+24".to_string());
        }
        if source_handle_fresh {
            score += 18;
            reasons.push("source_handle_fresh+18".to_string());
        }
        if duplicate_name_precision + f64::EPSILON >= COMMUNITY_MIN_DUPLICATE_NAME_PRECISION {
            score += 10;
            reasons.push("duplicate_name_precision+10".to_string());
        } else {
            score -= 10;
            reasons.push("duplicate_name_precision-10".to_string());
        }
        if page_handle_coverage_pct + f64::EPSILON >= COMMUNITY_MIN_HANDLE_COVERAGE_PCT {
            score += 10;
            reasons.push("handle_coverage+10".to_string());
        } else {
            score -= 10;
            reasons.push("handle_coverage-10".to_string());
        }

        ranked.push(RankedNeighbor {
            rank: 0,
            node_id: node.id.clone(),
            kind: node.kind.clone(),
            label: node.label.clone(),
            score,
            depth,
            edge_kinds,
            community_co_membership,
            semantic_relation,
            source_handle_fresh,
            duplicate_name_precision,
            handle_coverage_pct: page_handle_coverage_pct,
            reasons,
        });
    }

    ranked.sort_by(|left, right| {
        right
            .score
            .cmp(&left.score)
            .then_with(|| {
                left.depth
                    .unwrap_or(usize::MAX)
                    .cmp(&right.depth.unwrap_or(usize::MAX))
            })
            .then(left.kind.cmp(&right.kind))
            .then(left.label.cmp(&right.label))
            .then(left.node_id.cmp(&right.node_id))
    });
    for (idx, neighbor) in ranked.iter_mut().enumerate() {
        neighbor.rank = idx + 1;
    }
    ranked
}

pub fn neighborhood_depths(
    center_id: &str,
    node_by_id: &BTreeMap<String, &GraphNode>,
    outgoing: &BTreeMap<String, Vec<&GraphEdge>>,
) -> BTreeMap<String, usize> {
    if !node_by_id.contains_key(center_id) {
        return BTreeMap::new();
    }
    let mut depths = BTreeMap::from([(center_id.to_string(), 0usize)]);
    let mut queue = VecDeque::from([(center_id.to_string(), 0usize)]);
    while let Some((current, depth)) = queue.pop_front() {
        for edge in outgoing.get(&current).into_iter().flatten() {
            if !node_by_id.contains_key(&edge.to_id) || depths.contains_key(&edge.to_id) {
                continue;
            }
            let next_depth = depth + 1;
            depths.insert(edge.to_id.clone(), next_depth);
            queue.push_back((edge.to_id.clone(), next_depth));
        }
    }
    depths
}

pub fn page_handle_coverage_pct(nodes: &[GraphNode]) -> f64 {
    if nodes.is_empty() {
        return 0.0;
    }
    let covered = nodes.iter().filter(|node| node_has_handle_coverage(node)).count();
    (covered as f64 / nodes.len() as f64) * 100.0
}

pub fn node_has_handle_coverage(node: &GraphNode) -> bool {
    !node.id.is_empty()
        || node.properties.contains_key("handle")
        || node.properties.contains_key("ref_id")
        || node.properties.contains_key("tagpath_handle")
}

pub fn duplicate_name_precision(node: &GraphNode, label_counts: &BTreeMap<String, usize>) -> f64 {
    let count = label_counts.get(&node.label).copied().unwrap_or(1);
    if count <= 1 || node_has_handle_coverage(node) {
        1.0
    } else {
        1.0 / count as f64
    }
}

pub fn has_community_signal(node: &GraphNode, edge_kinds: &[String]) -> bool {
    node.kind.contains("community")
        || edge_kinds.iter().any(|kind| kind.contains("community"))
        || node
            .properties
            .iter()
            .any(|(key, value)| key.contains("community") || value.contains("community"))
}

pub fn has_semantic_signal(node: &GraphNode, edge_kinds: &[String]) -> bool {
    node.kind.starts_with("semantic_")
        || edge_kinds
            .iter()
            .any(|kind| kind.contains("semantic") || kind.contains("concept") || kind.contains("entity"))
}

pub fn source_handle_is_fresh(node: &GraphNode) -> bool {
    node.kind == "source_handle"
        && node.freshness.as_ref().is_some_and(|freshness| {
            freshness.content_hash.is_some() || freshness.observed_at_unix.is_some()
        })
}

pub fn edge_kind_rank_score(edge_kind: &str) -> i64 {
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

pub fn default_neighborhood_ranking_gate() -> NeighborhoodRankingGate {
    NeighborhoodRankingGate {
        status: "active".to_string(),
        ranked_output_default: true,
        default_order: "score_desc".to_string(),
        default_change_gate: "three_sample_regression".to_string(),
        required_workloads: vec![
            "real".to_string(),
            "full_projection".to_string(),
            "synthetic_high_degree".to_string(),
            "synthetic_deep_chain".to_string(),
        ],
        required_metrics: vec![
            "handle_coverage_pct".to_string(),
            "duplicate_name_precision".to_string(),
            "top_community_stability".to_string(),
        ],
        max_duration_regression_percent: 10.0,
        min_handle_coverage_pct: COMMUNITY_MIN_HANDLE_COVERAGE_PCT,
        min_duplicate_name_precision: COMMUNITY_MIN_DUPLICATE_NAME_PRECISION,
        min_top_community_stability: 0.95,
        diagnostics: vec![],
    }
}

#[cfg(test)]
mod tests {
    use super::*;
use tsift_core::{GraphEdge, GraphNode};

    #[test]
    fn edge_kind_rank_score_semantic_relation() {
        assert_eq!(edge_kind_rank_score("semantic_relation"), 34);
    }

    #[test]
    fn edge_kind_rank_score_calls() {
        assert_eq!(edge_kind_rank_score("calls"), 20);
    }

    #[test]
    fn edge_kind_rank_score_unknown() {
        assert_eq!(edge_kind_rank_score("unknown_kind"), 8);
    }

    #[test]
    fn edge_kind_rank_score_community_prefix() {
        assert_eq!(edge_kind_rank_score("community_member"), 20);
    }

    #[test]
    fn edge_kind_rank_score_concept_prefix() {
        assert_eq!(edge_kind_rank_score("concept_link"), 24);
    }

    #[test]
    fn neighborhood_depths_basic() {
        let nodes = vec![
            GraphNode::new("a", "file", "a"),
            GraphNode::new("b", "file", "b"),
            GraphNode::new("c", "file", "c"),
        ];
        let edges = vec![
            GraphEdge::new("a", "b", "calls"),
            GraphEdge::new("b", "c", "calls"),
        ];
        let node_by_id = nodes.iter().map(|n| (n.id.clone(), n)).collect();
        let outgoing: BTreeMap<String, Vec<&GraphEdge>> = edges
            .iter()
            .fold(BTreeMap::new(), |mut acc, e| {
                acc.entry(e.from_id.clone()).or_default().push(e);
                acc
            });
        let depths = neighborhood_depths("a", &node_by_id, &outgoing);
        assert_eq!(depths.get("a"), Some(&0));
        assert_eq!(depths.get("b"), Some(&1));
        assert_eq!(depths.get("c"), Some(&2));
    }

    #[test]
    fn neighborhood_depths_missing_center() {
        let nodes = vec![GraphNode::new("a", "file", "a")];
        let node_by_id = nodes.iter().map(|n| (n.id.clone(), n)).collect();
        let outgoing = BTreeMap::new();
        let depths = neighborhood_depths("missing", &node_by_id, &outgoing);
        assert!(depths.is_empty());
    }

    #[test]
    fn ranked_neighbors_sorts_by_score() {
        let center = GraphNode::new("center", "file", "center");
        let far = GraphNode::new("far", "symbol", "far_node");
        let near = GraphNode::new("near", "symbol", "near_node");
        let nodes = vec![center, far, near];
        let edges = vec![
            GraphEdge::new("center", "near", "calls"),
            GraphEdge::new("near", "far", "calls"),
        ];
        let ranked = ranked_neighbors("center", &nodes, &edges);
        assert_eq!(ranked.len(), 2);
        assert!(ranked[0].score >= ranked[1].score);
        assert_eq!(ranked[0].rank, 1);
        assert_eq!(ranked[1].rank, 2);
    }

    #[test]
    fn ranked_neighbors_skips_center() {
        let center = GraphNode::new("center", "file", "center");
        let other = GraphNode::new("other", "symbol", "other");
        let nodes = vec![center, other];
        let ranked = ranked_neighbors("center", &nodes, &[]);
        assert_eq!(ranked.len(), 1);
        assert_eq!(ranked[0].node_id, "other");
    }

    #[test]
    fn page_handle_coverage_pct_empty() {
        assert_eq!(page_handle_coverage_pct(&[]), 0.0);
    }

    #[test]
    fn page_handle_coverage_pct_all_covered() {
        let nodes = vec![
            GraphNode::new("a", "file", "a").with_property("handle", "h:1"),
            GraphNode::new("b", "file", "b").with_property("ref_id", "#1"),
        ];
        let pct = page_handle_coverage_pct(&nodes);
        assert!(pct > 99.0);
    }

    #[test]
    fn page_handle_coverage_pct_partial() {
        let nodes = vec![
            GraphNode::new("a", "file", "a").with_property("handle", "h:1"),
            GraphNode::new("", "file", "b"),
        ];
        let pct = page_handle_coverage_pct(&nodes);
        assert!((pct - 50.0).abs() < 1e-9);
    }

    #[test]
    fn duplicate_name_precision_unique() {
        let node = GraphNode::new("a", "symbol", "unique_name");
        let counts = BTreeMap::from([("unique_name".to_string(), 1)]);
        assert_eq!(duplicate_name_precision(&node, &counts), 1.0);
    }

    #[test]
    fn duplicate_name_precision_duplicate_with_handle() {
        let node =
            GraphNode::new("a", "symbol", "dup").with_property("handle", "h:1");
        let counts = BTreeMap::from([("dup".to_string(), 5)]);
        assert_eq!(duplicate_name_precision(&node, &counts), 1.0);
    }

    #[test]
    fn duplicate_name_precision_duplicate_without_handle() {
        let node = GraphNode::new("", "symbol", "dup");
        let counts = BTreeMap::from([("dup".to_string(), 4)]);
        assert!((duplicate_name_precision(&node, &counts) - 0.25).abs() < 1e-9);
    }

    #[test]
    fn has_community_signal_kind() {
        let node = GraphNode::new("a", "community_member", "a");
        assert!(has_community_signal(&node, &[]));
    }

    #[test]
    fn has_community_signal_edge_kind() {
        let node = GraphNode::new("a", "file", "a");
        assert!(has_community_signal(&node, &["community_link".to_string()]));
    }

    #[test]
    fn has_community_signal_property() {
        let node =
            GraphNode::new("a", "file", "a").with_property("community_id", "c1");
        assert!(has_community_signal(&node, &[]));
    }

    #[test]
    fn has_semantic_signal_kind() {
        let node = GraphNode::new("a", "semantic_concept", "a");
        assert!(has_semantic_signal(&node, &[]));
    }

    #[test]
    fn has_semantic_signal_edge_kind() {
        let node = GraphNode::new("a", "file", "a");
        assert!(has_semantic_signal(&node, &["concept_link".to_string()]));
    }

    #[test]
    fn source_handle_is_fresh_true() {
        let node = GraphNode::new("a", "source_handle", "a")
            .with_freshness(GraphFreshness::content_hash("abc"));
        assert!(source_handle_is_fresh(&node));
    }

    #[test]
    fn source_handle_is_fresh_wrong_kind() {
        let node = GraphNode::new("a", "file", "a")
            .with_freshness(GraphFreshness::content_hash("abc"));
        assert!(!source_handle_is_fresh(&node));
    }

    #[test]
    fn source_handle_is_fresh_no_freshness() {
        let node = GraphNode::new("a", "source_handle", "a");
        assert!(!source_handle_is_fresh(&node));
    }

    #[test]
    fn node_has_handle_coverage_handle() {
        let node = GraphNode::new("a", "file", "a").with_property("handle", "h:1");
        assert!(node_has_handle_coverage(&node));
    }

    #[test]
    fn node_has_handle_coverage_ref_id() {
        let node = GraphNode::new("a", "file", "a").with_property("ref_id", "#1");
        assert!(node_has_handle_coverage(&node));
    }

    #[test]
    fn node_has_handle_coverage_empty() {
        let node = GraphNode::new("", "file", "a");
        assert!(!node_has_handle_coverage(&node));
    }

    #[test]
    fn default_neighborhood_ranking_gate_values() {
        let gate = default_neighborhood_ranking_gate();
        assert_eq!(gate.status, "active");
        assert!(gate.ranked_output_default);
        assert_eq!(gate.min_handle_coverage_pct, COMMUNITY_MIN_HANDLE_COVERAGE_PCT);
        assert_eq!(gate.min_duplicate_name_precision, COMMUNITY_MIN_DUPLICATE_NAME_PRECISION);
    }
}
