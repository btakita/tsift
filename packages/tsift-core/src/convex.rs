use anyhow::{Result, bail};
use serde::{Deserialize, Serialize};
use std::cell::RefCell;
use std::collections::BTreeMap;

use crate::store::{GraphStore, shortest_path_using_outgoing};
use crate::types::*;

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
