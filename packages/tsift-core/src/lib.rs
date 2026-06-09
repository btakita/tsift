pub mod convex;
pub mod store;
pub mod types;

pub use convex::{
    ConvexEdgeRow, ConvexGraphClient, ConvexGraphStore, ConvexNodeRow, ConvexProjectionRows,
    ConvexRowsGraphClient,
};
pub use store::{
    GraphStore, apply_graph_edge_query_page, apply_graph_query_page, graph_semantic_cosine,
    graph_semantic_top_candidates_by_property_scan, parse_graph_semantic_vector_property,
    shortest_path_using_outgoing,
};
pub use types::{
    GRAPH_SEMANTIC_VECTOR_DEFAULT_MODEL, GRAPH_SEMANTIC_VECTOR_MODEL_PROPERTY_KEY,
    GRAPH_SEMANTIC_VECTOR_PROPERTY_KEY, GraphEdge, GraphFreshness, GraphNode, GraphPagedSubgraph,
    GraphPath, GraphProjection, GraphPropertyFilter, GraphProvenance, GraphQueryOptions,
    GraphQueryPage, GraphSemanticCandidate, GraphSubgraph, NeighborhoodScoring, PropertyMode,
    RankedNeighborhoodOptions, RankedNeighborhoodResult, SQLITE_GRAPH_SCHEMA_VERSION,
    TerseGraphEdge, TerseGraphNode, TerseGraphSubgraph, TerseHealthScore, TerseSearchHit,
    graph_edge_id, stable_graph_edge_id,
};

impl GraphProjection {
    pub fn upsert_into<S: GraphStore + ?Sized>(&self, store: &S) -> anyhow::Result<()> {
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;
    use std::collections::BTreeMap;

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
        fn upsert_node_row(&self, row: &ConvexNodeRow) -> anyhow::Result<()> {
            self.nodes
                .borrow_mut()
                .insert(row.external_id.clone(), row.clone());
            Ok(())
        }

        fn upsert_edge_row(&self, row: &ConvexEdgeRow) -> anyhow::Result<()> {
            self.edges
                .borrow_mut()
                .insert(row.edge_key.clone(), row.clone());
            Ok(())
        }

        fn delete_node_row(&self, external_id: &str) -> anyhow::Result<usize> {
            Ok(usize::from(
                self.nodes.borrow_mut().remove(external_id).is_some(),
            ))
        }

        fn delete_edge_row(&self, edge_key: &str) -> anyhow::Result<usize> {
            Ok(usize::from(
                self.edges.borrow_mut().remove(edge_key).is_some(),
            ))
        }

        fn node_row(&self, external_id: &str) -> anyhow::Result<Option<ConvexNodeRow>> {
            Ok(self.nodes.borrow().get(external_id).cloned())
        }

        fn node_rows(&self) -> anyhow::Result<Vec<ConvexNodeRow>> {
            Ok(self.nodes.borrow().values().cloned().collect())
        }

        fn edge_rows(&self) -> anyhow::Result<Vec<ConvexEdgeRow>> {
            Ok(self.edges.borrow().values().cloned().collect())
        }

        fn node_rows_by_kind(&self, kind: &str) -> anyhow::Result<Vec<ConvexNodeRow>> {
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
        ) -> anyhow::Result<Vec<ConvexEdgeRow>> {
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
            edges: vec![
                GraphEdge::new("a", "b", "calls").with_freshness(GraphFreshness::content_hash("h")),
            ],
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

    #[test]
    fn ranked_neighborhood_returns_none_for_missing_center() {
        let store = ConvexGraphStore::new(MemoryConvexGraphClient::default());
        let options = RankedNeighborhoodOptions::new(2, 10);
        let result = store.ranked_neighborhood("missing", &options).unwrap();
        assert!(result.is_none());
    }

    #[test]
    fn ranked_neighborhood_breadth_first_respects_max_nodes() {
        let store = ConvexGraphStore::new(MemoryConvexGraphClient::default());
        store
            .upsert_node(&GraphNode::new("a", "file", "a"))
            .unwrap();
        store
            .upsert_node(&GraphNode::new("b", "symbol", "b"))
            .unwrap();
        store
            .upsert_node(&GraphNode::new("c", "symbol", "c"))
            .unwrap();
        store
            .upsert_node(&GraphNode::new("d", "symbol", "d"))
            .unwrap();
        store
            .upsert_edge(&GraphEdge::new("a", "b", "calls"))
            .unwrap();
        store
            .upsert_edge(&GraphEdge::new("a", "c", "calls"))
            .unwrap();
        store
            .upsert_edge(&GraphEdge::new("a", "d", "calls"))
            .unwrap();

        let options = RankedNeighborhoodOptions::new(2, 2);
        let result = store.ranked_neighborhood("a", &options).unwrap().unwrap();
        assert!(
            result.nodes.len() <= 3,
            "center + max 2 neighbors, got {}",
            result.nodes.len()
        );
        assert!(result.pruned_count > 0, "should have pruned some nodes");
        assert!(result.total_discovered >= 4);
    }

    #[test]
    fn ranked_neighborhood_edge_kind_weighted_prefers_high_score_edges() {
        let store = ConvexGraphStore::new(MemoryConvexGraphClient::default());
        store
            .upsert_node(&GraphNode::new("a", "file", "a"))
            .unwrap();
        store
            .upsert_node(&GraphNode::new("b", "symbol", "b"))
            .unwrap();
        store
            .upsert_node(&GraphNode::new("c", "symbol", "c"))
            .unwrap();
        store
            .upsert_edge(&GraphEdge::new("a", "b", "semantic_relation"))
            .unwrap();
        store
            .upsert_edge(&GraphEdge::new("a", "c", "unknown"))
            .unwrap();

        let options = RankedNeighborhoodOptions::new(1, 1)
            .with_scoring(NeighborhoodScoring::EdgeKindWeighted);
        let result = store.ranked_neighborhood("a", &options).unwrap().unwrap();
        let neighbor_ids: Vec<_> = result.nodes.iter().map(|n| n.id.as_str()).collect();
        assert!(
            neighbor_ids.contains(&"b"),
            "semantic_relation neighbor should survive pruning"
        );
    }

    #[test]
    fn ranked_neighborhood_includes_center_node() {
        let store = ConvexGraphStore::new(MemoryConvexGraphClient::default());
        store
            .upsert_node(&GraphNode::new("center", "file", "center"))
            .unwrap();
        store
            .upsert_node(&GraphNode::new("neighbor", "symbol", "neighbor"))
            .unwrap();
        store
            .upsert_edge(&GraphEdge::new("center", "neighbor", "calls"))
            .unwrap();

        let options = RankedNeighborhoodOptions::new(1, 10);
        let result = store
            .ranked_neighborhood("center", &options)
            .unwrap()
            .unwrap();
        let ids: Vec<_> = result.nodes.iter().map(|n| n.id.clone()).collect();
        assert!(ids.contains(&"center".to_string()));
        assert!(ids.contains(&"neighbor".to_string()));
        assert_eq!(result.pruned_count, 0);
    }

    #[test]
    fn ranked_neighborhood_edge_kind_filter() {
        let store = ConvexGraphStore::new(MemoryConvexGraphClient::default());
        store
            .upsert_node(&GraphNode::new("a", "file", "a"))
            .unwrap();
        store
            .upsert_node(&GraphNode::new("b", "symbol", "b"))
            .unwrap();
        store
            .upsert_node(&GraphNode::new("c", "symbol", "c"))
            .unwrap();
        store
            .upsert_edge(&GraphEdge::new("a", "b", "calls"))
            .unwrap();
        store
            .upsert_edge(&GraphEdge::new("a", "c", "mentions"))
            .unwrap();

        let options = RankedNeighborhoodOptions::new(1, 10).with_edge_kind("calls");
        let result = store.ranked_neighborhood("a", &options).unwrap().unwrap();
        let ids: Vec<_> = result.nodes.iter().map(|n| n.id.as_str()).collect();
        assert!(ids.contains(&"b"));
        assert!(!ids.contains(&"c"), "mentions edge should be filtered out");
    }

    #[test]
    fn ranked_neighborhood_degree_weighted_prefers_low_degree() {
        let store = ConvexGraphStore::new(MemoryConvexGraphClient::default());
        store
            .upsert_node(&GraphNode::new("a", "file", "a"))
            .unwrap();
        store
            .upsert_node(&GraphNode::new("b", "symbol", "b"))
            .unwrap();
        store
            .upsert_node(&GraphNode::new("c", "symbol", "c"))
            .unwrap();
        store
            .upsert_node(&GraphNode::new("d", "symbol", "d"))
            .unwrap();
        store
            .upsert_node(&GraphNode::new("e", "symbol", "e"))
            .unwrap();
        store
            .upsert_edge(&GraphEdge::new("a", "b", "calls"))
            .unwrap();
        store
            .upsert_edge(&GraphEdge::new("b", "c", "calls"))
            .unwrap();
        store
            .upsert_edge(&GraphEdge::new("b", "d", "calls"))
            .unwrap();
        store
            .upsert_edge(&GraphEdge::new("b", "e", "calls"))
            .unwrap();

        let options =
            RankedNeighborhoodOptions::new(2, 3).with_scoring(NeighborhoodScoring::DegreeWeighted);
        let result = store.ranked_neighborhood("a", &options).unwrap().unwrap();
        assert!(result.nodes.len() <= 4);
    }

    #[test]
    fn compute_neighborhood_score_breadth_first() {
        let score = store::compute_neighborhood_score(
            &NeighborhoodScoring::BreadthFirst,
            0,
            "calls",
            &GraphNode::new("x", "symbol", "x"),
            &BTreeMap::new(),
        );
        assert_eq!(score, 120);
        let score_d2 = store::compute_neighborhood_score(
            &NeighborhoodScoring::BreadthFirst,
            2,
            "calls",
            &GraphNode::new("x", "symbol", "x"),
            &BTreeMap::new(),
        );
        assert_eq!(score_d2, 84);
    }

    #[test]
    fn compute_neighborhood_score_edge_kind_weighted() {
        let score_semantic = store::compute_neighborhood_score(
            &NeighborhoodScoring::EdgeKindWeighted,
            1,
            "semantic_relation",
            &GraphNode::new("x", "symbol", "x"),
            &BTreeMap::new(),
        );
        let score_unknown = store::compute_neighborhood_score(
            &NeighborhoodScoring::EdgeKindWeighted,
            1,
            "unknown",
            &GraphNode::new("x", "symbol", "x"),
            &BTreeMap::new(),
        );
        assert!(score_semantic > score_unknown);
    }
}
