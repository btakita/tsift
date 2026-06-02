use anyhow::{Context, Result, anyhow};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;
use std::sync::RwLock;
use surrealdb::Surreal;
use surrealdb::engine::local::{Db, Mem, SurrealKv};
use tsift_core::{
    ConvexEdgeRow, ConvexNodeRow, ConvexProjectionRows, GraphEdge, GraphNode, GraphPagedSubgraph,
    GraphPath, GraphPropertyFilter, GraphQueryOptions, GraphStore, apply_graph_edge_query_page,
    graph_edge_id, shortest_path_using_outgoing, stable_graph_edge_id,
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

type DirectionEdgeIndex = BTreeMap<String, BTreeMap<String, BTreeMap<(String, String), String>>>;

#[derive(Default)]
struct SurrealEdgeIndexes {
    by_id: BTreeMap<String, GraphEdge>,
    ordered: BTreeMap<(String, String, String, String), String>,
    by_kind: BTreeMap<String, BTreeMap<String, String>>,
    by_kind_order: BTreeMap<String, BTreeMap<(String, String, String), String>>,
    outgoing: DirectionEdgeIndex,
    incoming: DirectionEdgeIndex,
    by_property: BTreeMap<(String, String), BTreeMap<String, String>>,
}

impl SurrealEdgeIndexes {
    fn len(&self) -> usize {
        self.by_id.len()
    }

    fn clear(&mut self) {
        self.by_id.clear();
        self.ordered.clear();
        self.by_kind.clear();
        self.by_kind_order.clear();
        self.outgoing.clear();
        self.incoming.clear();
        self.by_property.clear();
    }

    fn insert(&mut self, edge: GraphEdge) {
        let edge_id = graph_edge_id(&edge);
        if let Some(previous) = self.by_id.remove(&edge_id) {
            self.remove_index_entries(&edge_id, &previous);
        }
        self.insert_index_entries(&edge_id, &edge);
        self.by_id.insert(edge_id, edge);
    }

    fn remove(&mut self, edge_id: &str) -> Option<GraphEdge> {
        let edge = self.by_id.remove(edge_id)?;
        self.remove_index_entries(edge_id, &edge);
        Some(edge)
    }

    fn edge(&self, edge_id: &str) -> Option<GraphEdge> {
        self.by_id.get(edge_id).cloned()
    }

    fn ordered_edges(&self) -> Vec<GraphEdge> {
        self.ordered
            .values()
            .filter_map(|edge_id| self.by_id.get(edge_id))
            .cloned()
            .collect()
    }

    fn sample_edge(&self, kind: Option<&str>) -> Option<GraphEdge> {
        match kind {
            Some(kind) => {
                let ordered = self.by_kind_order.get(kind)?;
                for edge_id in ordered.values() {
                    let Some(edge) = self.by_id.get(edge_id) else {
                        continue;
                    };
                    if edge.from_id != edge.to_id {
                        return Some(edge.clone());
                    }
                }
            }
            None => {
                for edge_id in self.ordered.values() {
                    let Some(edge) = self.by_id.get(edge_id) else {
                        continue;
                    };
                    if edge.from_id != edge.to_id {
                        return Some(edge.clone());
                    }
                }
            }
        }
        None
    }

    fn sample_edge_with_property(&self) -> Option<(GraphEdge, GraphPropertyFilter)> {
        for ((key, value), edge_ids) in &self.by_property {
            for edge_id in edge_ids.values() {
                let Some(edge) = self.by_id.get(edge_id) else {
                    continue;
                };
                if edge.from_id == edge.to_id {
                    continue;
                }
                return Some((
                    edge.clone(),
                    GraphPropertyFilter {
                        key: key.clone(),
                        value: value.clone(),
                    },
                ));
            }
        }
        None
    }

    fn outgoing_edges(&self, from_id: &str, kind: Option<&str>) -> Vec<GraphEdge> {
        let Some(kind_edges) = self.outgoing.get(from_id) else {
            return Vec::new();
        };
        let mut edges = Vec::new();
        match kind {
            Some(kind) => {
                if let Some(ids) = kind_edges.get(kind) {
                    edges.extend(
                        ids.values()
                            .filter_map(|edge_id| self.by_id.get(edge_id).cloned()),
                    );
                }
            }
            None => {
                for ids in kind_edges.values() {
                    edges.extend(
                        ids.values()
                            .filter_map(|edge_id| self.by_id.get(edge_id).cloned()),
                    );
                }
                edges.sort_by(|left, right| {
                    left.to_id
                        .cmp(&right.to_id)
                        .then(left.kind.cmp(&right.kind))
                        .then_with(|| graph_edge_id(left).cmp(&graph_edge_id(right)))
                });
            }
        }
        edges
    }

    fn incident_edges(&self, node_id: &str, kind: Option<&str>) -> Vec<GraphEdge> {
        let mut edge_ids = BTreeSet::new();
        if let Some(kind_edges) = self.outgoing.get(node_id) {
            Self::collect_direction_edge_ids(kind_edges, kind, &mut edge_ids);
        }
        if let Some(kind_edges) = self.incoming.get(node_id) {
            Self::collect_direction_edge_ids(kind_edges, kind, &mut edge_ids);
        }
        edge_ids
            .into_iter()
            .filter_map(|edge_id| self.by_id.get(&edge_id).cloned())
            .collect()
    }

    fn paged_edge_candidates(
        &self,
        kind: Option<&str>,
        property_filters: &[GraphPropertyFilter],
    ) -> Vec<GraphEdge> {
        let edge_ids = if let Some(primary_filter) = property_filters.first() {
            self.by_property
                .get(&(primary_filter.key.clone(), primary_filter.value.clone()))
                .map(|ids| ids.values().cloned().collect::<Vec<_>>())
                .unwrap_or_default()
        } else if let Some(kind) = kind {
            self.by_kind
                .get(kind)
                .map(|ids| ids.values().cloned().collect::<Vec<_>>())
                .unwrap_or_default()
        } else {
            self.by_id.keys().cloned().collect()
        };

        edge_ids
            .into_iter()
            .filter_map(|edge_id| self.by_id.get(&edge_id))
            .filter(|edge| kind.is_none_or(|kind| edge.kind == kind))
            .cloned()
            .collect()
    }

    fn insert_index_entries(&mut self, edge_id: &str, edge: &GraphEdge) {
        self.ordered.insert(
            (
                edge.from_id.clone(),
                edge.kind.clone(),
                edge.to_id.clone(),
                edge_id.to_string(),
            ),
            edge_id.to_string(),
        );
        self.by_kind
            .entry(edge.kind.clone())
            .or_default()
            .insert(edge_id.to_string(), edge_id.to_string());
        self.by_kind_order
            .entry(edge.kind.clone())
            .or_default()
            .insert(
                (
                    edge.from_id.clone(),
                    edge.to_id.clone(),
                    edge_id.to_string(),
                ),
                edge_id.to_string(),
            );
        self.outgoing
            .entry(edge.from_id.clone())
            .or_default()
            .entry(edge.kind.clone())
            .or_default()
            .insert(
                (edge.to_id.clone(), edge_id.to_string()),
                edge_id.to_string(),
            );
        self.incoming
            .entry(edge.to_id.clone())
            .or_default()
            .entry(edge.kind.clone())
            .or_default()
            .insert(
                (edge.from_id.clone(), edge_id.to_string()),
                edge_id.to_string(),
            );
        for (key, value) in &edge.properties {
            self.by_property
                .entry((key.clone(), value.clone()))
                .or_default()
                .insert(edge_id.to_string(), edge_id.to_string());
        }
    }

    fn remove_index_entries(&mut self, edge_id: &str, edge: &GraphEdge) {
        self.ordered.remove(&(
            edge.from_id.clone(),
            edge.kind.clone(),
            edge.to_id.clone(),
            edge_id.to_string(),
        ));
        Self::remove_kind_entry(&mut self.by_kind, &edge.kind, edge_id);
        Self::remove_kind_order_entry(
            &mut self.by_kind_order,
            &edge.kind,
            &(
                edge.from_id.clone(),
                edge.to_id.clone(),
                edge_id.to_string(),
            ),
        );
        Self::remove_direction_entry(
            &mut self.outgoing,
            &edge.from_id,
            &edge.kind,
            &(edge.to_id.clone(), edge_id.to_string()),
        );
        Self::remove_direction_entry(
            &mut self.incoming,
            &edge.to_id,
            &edge.kind,
            &(edge.from_id.clone(), edge_id.to_string()),
        );
        for (key, value) in &edge.properties {
            Self::remove_property_entry(
                &mut self.by_property,
                &(key.clone(), value.clone()),
                edge_id,
            );
        }
    }

    fn collect_direction_edge_ids(
        kind_edges: &BTreeMap<String, BTreeMap<(String, String), String>>,
        kind: Option<&str>,
        edge_ids: &mut BTreeSet<String>,
    ) {
        match kind {
            Some(kind) => {
                if let Some(ids) = kind_edges.get(kind) {
                    edge_ids.extend(ids.values().cloned());
                }
            }
            None => {
                for ids in kind_edges.values() {
                    edge_ids.extend(ids.values().cloned());
                }
            }
        }
    }

    fn remove_kind_entry(
        by_kind: &mut BTreeMap<String, BTreeMap<String, String>>,
        kind: &str,
        edge_id: &str,
    ) {
        let remove_kind = if let Some(edge_ids) = by_kind.get_mut(kind) {
            edge_ids.remove(edge_id);
            edge_ids.is_empty()
        } else {
            false
        };
        if remove_kind {
            by_kind.remove(kind);
        }
    }

    fn remove_kind_order_entry(
        by_kind_order: &mut BTreeMap<String, BTreeMap<(String, String, String), String>>,
        kind: &str,
        key: &(String, String, String),
    ) {
        let remove_kind = if let Some(edge_ids) = by_kind_order.get_mut(kind) {
            edge_ids.remove(key);
            edge_ids.is_empty()
        } else {
            false
        };
        if remove_kind {
            by_kind_order.remove(kind);
        }
    }

    fn remove_direction_entry(
        index: &mut DirectionEdgeIndex,
        node_id: &str,
        kind: &str,
        key: &(String, String),
    ) {
        let remove_node = if let Some(kind_edges) = index.get_mut(node_id) {
            let remove_kind = if let Some(edge_ids) = kind_edges.get_mut(kind) {
                edge_ids.remove(key);
                edge_ids.is_empty()
            } else {
                false
            };
            if remove_kind {
                kind_edges.remove(kind);
            }
            kind_edges.is_empty()
        } else {
            false
        };
        if remove_node {
            index.remove(node_id);
        }
    }

    fn remove_property_entry(
        by_property: &mut BTreeMap<(String, String), BTreeMap<String, String>>,
        property: &(String, String),
        edge_id: &str,
    ) {
        let remove_property = if let Some(edge_ids) = by_property.get_mut(property) {
            edge_ids.remove(edge_id);
            edge_ids.is_empty()
        } else {
            false
        };
        if remove_property {
            by_property.remove(property);
        }
    }
}

pub struct SurrealdbGraphStore {
    db: Surreal<Db>,
    rt: tokio::runtime::Runtime,
    nodes: RwLock<BTreeMap<String, GraphNode>>,
    edges: RwLock<SurrealEdgeIndexes>,
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
            edges: RwLock::new(SurrealEdgeIndexes::default()),
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
            edges: RwLock::new(SurrealEdgeIndexes::default()),
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
            edge_index.insert(edge);
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

    fn edges_read(&self) -> Result<std::sync::RwLockReadGuard<'_, SurrealEdgeIndexes>> {
        self.edges
            .read()
            .map_err(|_| anyhow!("SurrealDB graph edge index lock poisoned"))
    }

    fn edges_write(&self) -> Result<std::sync::RwLockWriteGuard<'_, SurrealEdgeIndexes>> {
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
        self.edges_write()?.insert(edge.clone());
        Ok(())
    }

    fn delete_node(&self, id: &str) -> Result<usize> {
        let incident = self
            .incident_edges(id, None)?
            .into_iter()
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
        Ok(self.edges_read()?.ordered_edges())
    }

    fn edge(&self, edge_id: &str) -> Result<Option<GraphEdge>> {
        Ok(self.edges_read()?.edge(edge_id))
    }

    fn graph_counts(&self) -> Result<(usize, usize)> {
        Ok((self.nodes_read()?.len(), self.edges_read()?.len()))
    }

    fn sample_edge(&self, kind: Option<&str>) -> Result<Option<GraphEdge>> {
        Ok(self.edges_read()?.sample_edge(kind))
    }

    fn sample_edge_with_property(&self) -> Result<Option<(GraphEdge, GraphPropertyFilter)>> {
        Ok(self.edges_read()?.sample_edge_with_property())
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
        Ok(self.edges_read()?.outgoing_edges(from_id, kind))
    }

    fn incident_edges(&self, node_id: &str, kind: Option<&str>) -> Result<Vec<GraphEdge>> {
        Ok(self.edges_read()?.incident_edges(node_id, kind))
    }

    fn paged_edges(
        &self,
        kind: Option<&str>,
        options: GraphQueryOptions,
    ) -> Result<GraphPagedSubgraph> {
        let candidates = self
            .edges_read()?
            .paged_edge_candidates(kind, &options.property_filters);
        let mut diagnostics = vec![
            "SurrealDB adapter uses Rust-side edge indexes before generic page filtering"
                .to_string(),
        ];
        if !options.property_filters.is_empty() {
            diagnostics.push(
                "primary edge-property filter probes the SurrealDB derived property index"
                    .to_string(),
            );
        } else if kind.is_some() {
            diagnostics
                .push("edge-kind filter probes the SurrealDB derived kind index".to_string());
        }
        Ok(apply_graph_edge_query_page(
            candidates,
            options,
            diagnostics,
        ))
    }

    fn paged_incident_edges(
        &self,
        node_id: &str,
        kind: Option<&str>,
        options: GraphQueryOptions,
    ) -> Result<GraphPagedSubgraph> {
        let candidates = self.edges_read()?.incident_edges(node_id, kind);
        Ok(apply_graph_edge_query_page(
            candidates,
            options,
            vec![
                "SurrealDB adapter uses Rust-side incoming/outgoing indexes before generic page filtering"
                    .to_string(),
            ],
        ))
    }

    fn edges_between_nodes(&self, node_ids: &BTreeSet<String>) -> Result<Vec<GraphEdge>> {
        if node_ids.is_empty() {
            return Ok(Vec::new());
        }
        let edges = self.edges_read()?;
        let mut between = BTreeMap::<(String, String, String), GraphEdge>::new();
        for from_id in node_ids {
            for edge in edges.outgoing_edges(from_id, None) {
                if node_ids.contains(&edge.to_id) {
                    between
                        .entry((edge.from_id.clone(), edge.kind.clone(), edge.to_id.clone()))
                        .or_insert(edge);
                }
            }
        }
        Ok(between.into_values().collect())
    }

    fn shortest_path(
        &self,
        from_id: &str,
        to_id: &str,
        kind: Option<&str>,
    ) -> Result<Option<GraphPath>> {
        self.shortest_path_with_max_hops(from_id, to_id, kind, None)
    }

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
}

#[cfg(test)]
mod tests {
    use super::*;
    use tsift_core::{GraphEdge, GraphNode, GraphPropertyFilter, GraphQueryOptions};

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

    #[test]
    fn surrealdb_store_uses_derived_edge_indexes_for_hot_reads() {
        let store = SurrealdbGraphStore::in_memory().unwrap();
        for id in ["node:a", "node:b", "node:c", "node:d"] {
            store
                .upsert_node(&GraphNode::new(id, "symbol", id))
                .unwrap();
        }
        let edge_ab = GraphEdge::new("node:a", "node:b", "calls").with_property("role", "new");
        let edge_ac =
            GraphEdge::new("node:a", "node:c", "mentions").with_property("confidence", "high");
        let edge_bd = GraphEdge::new("node:b", "node:d", "calls");
        let edge_da = GraphEdge::new("node:d", "node:a", "calls").with_property("role", "return");
        for edge in [&edge_da, &edge_ac, &edge_bd, &edge_ab] {
            store.upsert_edge(edge).unwrap();
        }

        assert_eq!(
            store.edge(&graph_edge_id(&edge_ab)).unwrap().unwrap().to_id,
            "node:b"
        );
        assert_eq!(
            store.sample_edge(Some("mentions")).unwrap().unwrap().to_id,
            "node:c"
        );
        let (property_edge, filter) = store.sample_edge_with_property().unwrap().unwrap();
        assert_eq!(
            property_edge.properties.get(&filter.key),
            Some(&filter.value)
        );

        let incident = store.incident_edges("node:a", Some("calls")).unwrap();
        let mut expected_incident = vec![graph_edge_id(&edge_ab), graph_edge_id(&edge_da)];
        expected_incident.sort();
        assert_eq!(
            incident.iter().map(graph_edge_id).collect::<Vec<_>>(),
            expected_incident
        );

        let page = store
            .paged_edges(
                None,
                GraphQueryOptions {
                    property_filters: vec![GraphPropertyFilter {
                        key: "role".to_string(),
                        value: "new".to_string(),
                    }],
                    ..GraphQueryOptions::default()
                },
            )
            .unwrap();
        assert_eq!(page.edges, vec![edge_ab.clone()]);
        assert!(
            page.page
                .diagnostics
                .iter()
                .any(|diagnostic| { diagnostic.contains("derived property index") })
        );

        let calls_page = store
            .paged_edges(
                Some("calls"),
                GraphQueryOptions {
                    limit: Some(2),
                    ..GraphQueryOptions::default()
                },
            )
            .unwrap();
        assert_eq!(calls_page.edges.len(), 2);
        assert!(calls_page.page.next_cursor.is_some());

        let between = store
            .edges_between_nodes(&BTreeSet::from([
                "node:a".to_string(),
                "node:b".to_string(),
                "node:c".to_string(),
            ]))
            .unwrap();
        assert_eq!(
            between.iter().map(graph_edge_id).collect::<Vec<_>>(),
            vec![graph_edge_id(&edge_ab), graph_edge_id(&edge_ac)]
        );

        assert!(
            store
                .shortest_path_with_max_hops("node:a", "node:d", Some("calls"), Some(1))
                .unwrap()
                .is_none()
        );
        assert_eq!(
            store
                .shortest_path_with_max_hops("node:a", "node:d", Some("calls"), Some(2))
                .unwrap()
                .unwrap()
                .nodes,
            vec![
                "node:a".to_string(),
                "node:b".to_string(),
                "node:d".to_string()
            ]
        );
    }

    #[test]
    fn surrealdb_store_updates_and_deletes_edge_index_entries() {
        let store = SurrealdbGraphStore::in_memory().unwrap();
        for id in ["node:a", "node:b", "node:c"] {
            store
                .upsert_node(&GraphNode::new(id, "symbol", id))
                .unwrap();
        }
        let old_edge = GraphEdge::new("node:a", "node:b", "calls").with_property("role", "old");
        let new_edge = GraphEdge::new("node:a", "node:b", "calls").with_property("role", "new");
        let incoming = GraphEdge::new("node:c", "node:a", "calls");
        store.upsert_edge(&old_edge).unwrap();
        store.upsert_edge(&new_edge).unwrap();
        store.upsert_edge(&incoming).unwrap();

        let old_page = store
            .paged_edges(
                None,
                GraphQueryOptions {
                    property_filters: vec![GraphPropertyFilter {
                        key: "role".to_string(),
                        value: "old".to_string(),
                    }],
                    ..GraphQueryOptions::default()
                },
            )
            .unwrap();
        assert!(old_page.edges.is_empty());

        let new_page = store
            .paged_edges(
                None,
                GraphQueryOptions {
                    property_filters: vec![GraphPropertyFilter {
                        key: "role".to_string(),
                        value: "new".to_string(),
                    }],
                    ..GraphQueryOptions::default()
                },
            )
            .unwrap();
        assert_eq!(new_page.edges, vec![new_edge.clone()]);

        assert_eq!(store.delete_node("node:a").unwrap(), 1);
        assert!(store.edge(&graph_edge_id(&new_edge)).unwrap().is_none());
        assert!(store.edge(&graph_edge_id(&incoming)).unwrap().is_none());
        assert!(store.outgoing_edges("node:a", None).unwrap().is_empty());
        assert!(store.incident_edges("node:a", None).unwrap().is_empty());
    }
}
