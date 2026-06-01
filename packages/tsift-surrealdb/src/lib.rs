use anyhow::{Context, Result, anyhow};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::Path;
use std::sync::RwLock;
use surrealdb::Surreal;
use surrealdb::engine::local::{Db, Mem, SurrealKv};
use tsift_core::{
    ConvexEdgeRow, ConvexNodeRow, ConvexProjectionRows, GraphEdge, GraphNode, GraphPath,
    GraphStore, graph_edge_id, shortest_path_using_outgoing, stable_graph_edge_id,
};

const NAMESPACE: &str = "tsift";
const DATABASE: &str = "graph";
const NODE_TABLE: &str = "graph_node";
const EDGE_TABLE: &str = "graph_edge";

fn block_on<F: std::future::Future>(rt: &tokio::runtime::Runtime, f: F) -> F::Output {
    rt.block_on(f)
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct SurrealNodeRecord {
    external_id: String,
    kind: String,
    label: String,
    properties: std::collections::BTreeMap<String, String>,
    provenance: Vec<tsift_core::GraphProvenance>,
    freshness: Option<tsift_core::GraphFreshness>,
}

impl From<&GraphNode> for SurrealNodeRecord {
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

impl From<SurrealNodeRecord> for GraphNode {
    fn from(record: SurrealNodeRecord) -> Self {
        Self {
            id: record.external_id,
            kind: record.kind,
            label: record.label,
            properties: record.properties,
            provenance: record.provenance,
            freshness: record.freshness,
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct SurrealEdgeRecord {
    edge_key: String,
    from_external_id: String,
    to_external_id: String,
    kind: String,
    properties: std::collections::BTreeMap<String, String>,
    provenance: Vec<tsift_core::GraphProvenance>,
    freshness: Option<tsift_core::GraphFreshness>,
}

impl From<&GraphEdge> for SurrealEdgeRecord {
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

impl From<SurrealEdgeRecord> for GraphEdge {
    fn from(record: SurrealEdgeRecord) -> Self {
        Self {
            id: record.edge_key,
            from_id: record.from_external_id,
            to_id: record.to_external_id,
            kind: record.kind,
            properties: record.properties,
            provenance: record.provenance,
            freshness: record.freshness,
        }
    }
}

fn record_key(prefix: &str, id: &str) -> String {
    format!("{prefix}_{}", blake3::hash(id.as_bytes()).to_hex())
}

fn node_from_row(row: &ConvexNodeRow) -> GraphNode {
    GraphNode {
        id: row.external_id.clone(),
        kind: row.kind.clone(),
        label: row.label.clone(),
        properties: row.properties.clone(),
        provenance: row.provenance.clone(),
        freshness: row.freshness.clone(),
    }
}

fn edge_from_row(row: &ConvexEdgeRow) -> GraphEdge {
    GraphEdge {
        id: row.edge_key.clone(),
        from_id: row.from_external_id.clone(),
        to_id: row.to_external_id.clone(),
        kind: row.kind.clone(),
        properties: row.properties.clone(),
        provenance: row.provenance.clone(),
        freshness: row.freshness.clone(),
    }
}

pub struct SurrealdbGraphStore {
    db: Surreal<Db>,
    rt: tokio::runtime::Runtime,
    nodes: RwLock<BTreeMap<String, GraphNode>>,
    edges: RwLock<BTreeMap<String, GraphEdge>>,
}

impl SurrealdbGraphStore {
    pub fn open(path: &Path) -> Result<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).with_context(|| {
                format!(
                    "creating SurrealDB graph substrate dir: {}",
                    parent.display()
                )
            })?;
        }
        let rt = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .context("creating tokio runtime for SurrealDB")?;
        let db = block_on(&rt, async {
            let db = Surreal::new::<SurrealKv>(path).await.with_context(|| {
                format!("opening SurrealDB SurrealKV store: {}", path.display())
            })?;
            db.use_ns(NAMESPACE)
                .use_db(DATABASE)
                .await
                .context("selecting tsift SurrealDB namespace/database")?;
            Ok::<_, anyhow::Error>(db)
        })?;
        let store = Self {
            db,
            rt,
            nodes: RwLock::new(BTreeMap::new()),
            edges: RwLock::new(BTreeMap::new()),
        };
        store.load_indexes()?;
        Ok(store)
    }

    pub fn in_memory() -> Result<Self> {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .context("creating tokio runtime for in-memory SurrealDB")?;
        let db = block_on(&rt, async {
            let db = Surreal::new::<Mem>(())
                .await
                .context("opening in-memory SurrealDB graph store")?;
            db.use_ns(NAMESPACE)
                .use_db(DATABASE)
                .await
                .context("selecting tsift SurrealDB namespace/database")?;
            Ok::<_, anyhow::Error>(db)
        })?;
        let store = Self {
            db,
            rt,
            nodes: RwLock::new(BTreeMap::new()),
            edges: RwLock::new(BTreeMap::new()),
        };
        store.load_indexes()?;
        Ok(store)
    }

    pub fn replace_projection_rows(&self, rows: &ConvexProjectionRows) -> Result<usize> {
        self.clear()?;
        for node in &rows.nodes {
            self.upsert_node(&node_from_row(node))?;
        }
        for edge in &rows.edges {
            self.upsert_edge(&edge_from_row(edge))?;
        }
        Ok(rows.nodes.len() + rows.edges.len())
    }

    pub fn from_rows_file_backed(path: &Path, rows: &ConvexProjectionRows) -> Result<Self> {
        let store = Self::open(path)?;
        store.replace_projection_rows(rows)?;
        Ok(store)
    }

    pub fn clear(&self) -> Result<()> {
        block_on(&self.rt, async {
            self.db
                .delete::<Vec<SurrealEdgeRecord>>(EDGE_TABLE)
                .await
                .context("clearing SurrealDB graph edges")?;
            self.db
                .delete::<Vec<SurrealNodeRecord>>(NODE_TABLE)
                .await
                .context("clearing SurrealDB graph nodes")?;
            Ok::<(), anyhow::Error>(())
        })?;
        self.nodes_write()?.clear();
        self.edges_write()?.clear();
        Ok(())
    }

    fn load_indexes(&self) -> Result<()> {
        let (nodes, edges) = block_on(&self.rt, async {
            let node_records = self
                .db
                .select::<Vec<SurrealNodeRecord>>(NODE_TABLE)
                .await
                .context("loading SurrealDB graph node index")?;
            let edge_records = self
                .db
                .select::<Vec<SurrealEdgeRecord>>(EDGE_TABLE)
                .await
                .context("loading SurrealDB graph edge index")?;
            Ok::<_, anyhow::Error>((node_records, edge_records))
        })?;
        let mut node_index = self.nodes_write()?;
        node_index.clear();
        for record in nodes {
            let node = GraphNode::from(record);
            node_index.insert(node.id.clone(), node);
        }
        drop(node_index);

        let mut edge_index = self.edges_write()?;
        edge_index.clear();
        for record in edges {
            let edge = GraphEdge::from(record);
            edge_index.insert(graph_edge_id(&edge), edge);
        }
        Ok(())
    }

    fn nodes_read(&self) -> Result<std::sync::RwLockReadGuard<'_, BTreeMap<String, GraphNode>>> {
        self.nodes
            .read()
            .map_err(|_| anyhow!("SurrealDB graph node index lock poisoned"))
    }

    fn nodes_write(&self) -> Result<std::sync::RwLockWriteGuard<'_, BTreeMap<String, GraphNode>>> {
        self.nodes
            .write()
            .map_err(|_| anyhow!("SurrealDB graph node index lock poisoned"))
    }

    fn edges_read(&self) -> Result<std::sync::RwLockReadGuard<'_, BTreeMap<String, GraphEdge>>> {
        self.edges
            .read()
            .map_err(|_| anyhow!("SurrealDB graph edge index lock poisoned"))
    }

    fn edges_write(&self) -> Result<std::sync::RwLockWriteGuard<'_, BTreeMap<String, GraphEdge>>> {
        self.edges
            .write()
            .map_err(|_| anyhow!("SurrealDB graph edge index lock poisoned"))
    }
}

impl GraphStore for SurrealdbGraphStore {
    fn upsert_node(&self, node: &GraphNode) -> Result<()> {
        let record = SurrealNodeRecord::from(node);
        let key = record_key("node", &node.id);
        block_on(&self.rt, async {
            self.db
                .upsert::<Option<SurrealNodeRecord>>((NODE_TABLE, key))
                .content(record)
                .await
                .context("upserting SurrealDB graph node")?;
            Ok::<(), anyhow::Error>(())
        })?;
        self.nodes_write()?.insert(node.id.clone(), node.clone());
        Ok(())
    }

    fn upsert_edge(&self, edge: &GraphEdge) -> Result<()> {
        let edge_id = graph_edge_id(edge);
        let record = SurrealEdgeRecord::from(edge);
        let key = record_key("edge", &edge_id);
        block_on(&self.rt, async {
            self.db
                .upsert::<Option<SurrealEdgeRecord>>((EDGE_TABLE, key))
                .content(record)
                .await
                .context("upserting SurrealDB graph edge")?;
            Ok::<(), anyhow::Error>(())
        })?;
        self.edges_write()?.insert(edge_id, edge.clone());
        Ok(())
    }

    fn delete_node(&self, id: &str) -> Result<usize> {
        let incident = self
            .all_edges()?
            .into_iter()
            .filter(|edge| edge.from_id == id || edge.to_id == id)
            .map(|edge| graph_edge_id(&edge))
            .collect::<Vec<_>>();
        let mut deleted_incident = Vec::new();
        for edge_id in incident {
            let deleted = block_on(&self.rt, async {
                self.db
                    .delete::<Option<SurrealEdgeRecord>>((EDGE_TABLE, record_key("edge", &edge_id)))
                    .await
                    .with_context(|| format!("deleting SurrealDB incident edge {edge_id}"))
            })?;
            if deleted.is_some() {
                deleted_incident.push(edge_id);
            }
        }
        if !deleted_incident.is_empty() {
            let mut edges = self.edges_write()?;
            for edge_id in deleted_incident {
                edges.remove(&edge_id);
            }
        }
        let deleted = block_on(&self.rt, async {
            self.db
                .delete::<Option<SurrealNodeRecord>>((NODE_TABLE, record_key("node", id)))
                .await
                .with_context(|| format!("deleting SurrealDB graph node {id}"))
        })?;
        if deleted.is_some() {
            self.nodes_write()?.remove(id);
        }
        Ok(usize::from(deleted.is_some()))
    }

    fn delete_edge(&self, from_id: &str, to_id: &str, kind: &str) -> Result<usize> {
        let edge_id = stable_graph_edge_id(from_id, to_id, kind);
        let deleted = block_on(&self.rt, async {
            self.db
                .delete::<Option<SurrealEdgeRecord>>((EDGE_TABLE, record_key("edge", &edge_id)))
                .await
                .with_context(|| format!("deleting SurrealDB graph edge {edge_id}"))
        })?;
        if deleted.is_some() {
            self.edges_write()?.remove(&edge_id);
        }
        Ok(usize::from(deleted.is_some()))
    }

    fn node(&self, id: &str) -> Result<Option<GraphNode>> {
        Ok(self.nodes_read()?.get(id).cloned())
    }

    fn all_nodes(&self) -> Result<Vec<GraphNode>> {
        let mut nodes = self.nodes_read()?.values().cloned().collect::<Vec<_>>();
        nodes.sort_by(|left, right| left.id.cmp(&right.id));
        Ok(nodes)
    }

    fn all_edges(&self) -> Result<Vec<GraphEdge>> {
        let mut edges = self.edges_read()?.values().cloned().collect::<Vec<_>>();
        edges.sort_by(|left, right| {
            left.from_id
                .cmp(&right.from_id)
                .then(left.kind.cmp(&right.kind))
                .then(left.to_id.cmp(&right.to_id))
                .then_with(|| graph_edge_id(left).cmp(&graph_edge_id(right)))
        });
        Ok(edges)
    }

    fn graph_counts(&self) -> Result<(usize, usize)> {
        Ok((self.all_nodes()?.len(), self.all_edges()?.len()))
    }

    fn nodes_by_kind(&self, kind: &str) -> Result<Vec<GraphNode>> {
        let mut nodes = self
            .all_nodes()?
            .into_iter()
            .filter(|node| node.kind == kind)
            .collect::<Vec<_>>();
        nodes.sort_by(|left, right| left.id.cmp(&right.id));
        Ok(nodes)
    }

    fn outgoing_edges(&self, from_id: &str, kind: Option<&str>) -> Result<Vec<GraphEdge>> {
        let mut edges = self
            .all_edges()?
            .into_iter()
            .filter(|edge| edge.from_id == from_id)
            .filter(|edge| kind.is_none_or(|kind| edge.kind == kind))
            .collect::<Vec<_>>();
        edges.sort_by(|left, right| {
            left.to_id
                .cmp(&right.to_id)
                .then(left.kind.cmp(&right.kind))
                .then_with(|| graph_edge_id(left).cmp(&graph_edge_id(right)))
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

#[cfg(test)]
mod tests {
    use super::*;
    use tsift_core::{GraphEdge, GraphNode};

    fn sample_rows() -> ConvexProjectionRows {
        let node_a = GraphNode::new("node:a", "symbol", "alpha").with_property("path", "a.rs");
        let node_b = GraphNode::new("node:b", "symbol", "beta").with_property("path", "b.rs");
        let edge = GraphEdge::new("node:a", "node:b", "calls").with_property("kind", "direct");
        tsift_core::GraphProjection {
            nodes: vec![node_a, node_b],
            edges: vec![edge],
        }
        .to_convex_rows()
    }

    #[test]
    fn surrealdb_store_writes_provider_neutral_rows_file_backed() {
        let dir = tempfile::tempdir().unwrap();
        let store = SurrealdbGraphStore::from_rows_file_backed(
            &dir.path().join("surrealdb"),
            &sample_rows(),
        )
        .unwrap();
        assert_eq!(store.graph_counts().unwrap(), (2, 1));
        assert_eq!(store.nodes_by_kind("symbol").unwrap().len(), 2);
        let outgoing = store.outgoing_edges("node:a", Some("calls")).unwrap();
        assert_eq!(outgoing.len(), 1);
        assert_eq!(outgoing[0].to_id, "node:b");
        assert_eq!(
            store
                .shortest_path("node:a", "node:b", Some("calls"))
                .unwrap()
                .unwrap()
                .nodes,
            vec!["node:a".to_string(), "node:b".to_string()]
        );
    }

    #[test]
    fn surrealdb_store_supports_graphstore_crud() {
        let store = SurrealdbGraphStore::in_memory().unwrap();
        let node = GraphNode::new("node:a", "symbol", "alpha");
        store.upsert_node(&node).unwrap();
        assert_eq!(store.node("node:a").unwrap().unwrap().label, "alpha");
        assert_eq!(store.delete_node("node:a").unwrap(), 1);
        assert!(store.node("node:a").unwrap().is_none());
    }
}
