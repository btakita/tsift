use anyhow::{Context, Result, anyhow};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};
use surrealdb::RecordId;
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
const METADATA_TABLE: &str = "metadata";
const ROW_HASH_KEY: &str = "row_hash";

fn block_on<F: std::future::Future>(rt: &tokio::runtime::Runtime, f: F) -> F::Output {
    rt.block_on(f)
}

fn row_hash<T: Serialize>(value: &T) -> Result<String> {
    let payload = serde_json::to_vec(value)?;
    Ok(blake3::hash(&payload).to_hex().to_string())
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum WarmStartOutcome {
    CacheHit,
    Refreshed,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeltaRefreshStats {
    pub unchanged_nodes: usize,
    pub unchanged_edges: usize,
    pub changed_nodes: usize,
    pub changed_edges: usize,
    pub tombstoned_nodes: usize,
    pub tombstoned_edges: usize,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct MetadataRecord {
    value: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct SurrealNodeRecord {
    external_id: String,
    kind: String,
    label: String,
    properties: std::collections::BTreeMap<String, String>,
    provenance: Vec<tsift_core::GraphProvenance>,
    freshness: Option<tsift_core::GraphFreshness>,
    #[serde(default)]
    row_hash: Option<String>,
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
            row_hash: None,
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

#[derive(Clone, Debug, Serialize)]
struct SurrealNodeInsertRecord {
    id: RecordId,
    row_hash: String,
    #[serde(flatten)]
    record: SurrealNodeRecord,
}

impl From<&GraphNode> for SurrealNodeInsertRecord {
    fn from(node: &GraphNode) -> Self {
        Self {
            id: RecordId::from_table_key(NODE_TABLE, record_key("node", &node.id)),
            row_hash: row_hash(node).unwrap_or_default(),
            record: SurrealNodeRecord::from(node),
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
    #[serde(default)]
    row_hash: Option<String>,
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
            row_hash: None,
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

#[derive(Clone, Debug, Serialize)]
struct SurrealEdgeInsertRecord {
    id: RecordId,
    row_hash: String,
    #[serde(flatten)]
    record: SurrealEdgeRecord,
}

impl From<&GraphEdge> for SurrealEdgeInsertRecord {
    fn from(edge: &GraphEdge) -> Self {
        let edge_id = graph_edge_id(edge);
        Self {
            id: RecordId::from_table_key(EDGE_TABLE, record_key("edge", &edge_id)),
            row_hash: row_hash(edge).unwrap_or_default(),
            record: SurrealEdgeRecord::from(edge),
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

#[derive(Default, Serialize, Deserialize, Clone)]
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

    fn all_edge_keys(&self) -> BTreeSet<String> {
        self.by_id.keys().cloned().collect()
    }

    #[allow(dead_code)]
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

const SIDECAR_VERSION: u32 = 1;

#[derive(Serialize, Deserialize)]
struct SidecarData {
    version: u32,
    stored_row_hash: Option<String>,
    nodes: BTreeMap<String, GraphNode>,
    edges: Vec<GraphEdge>,
    node_row_hashes: BTreeMap<String, String>,
    edge_row_hashes: BTreeMap<String, String>,
}

fn sidecar_path(store_path: &Path) -> PathBuf {
    let mut sidecar = store_path.as_os_str().to_os_string();
    sidecar.push(".index-sidecar");
    PathBuf::from(sidecar)
}

pub struct SurrealdbGraphStore {
    db: Surreal<Db>,
    rt: Arc<tokio::runtime::Runtime>,
    path: Option<PathBuf>,
    nodes: RwLock<BTreeMap<String, GraphNode>>,
    edges: RwLock<SurrealEdgeIndexes>,
    node_row_hashes: RwLock<BTreeMap<String, String>>,
    edge_row_hashes: RwLock<BTreeMap<String, String>>,
}

fn create_runtime() -> Result<tokio::runtime::Runtime> {
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .context("creating tokio runtime for SurrealDB")
}

impl SurrealdbGraphStore {
    pub fn open(path: &Path) -> Result<Self> {
        let rt = Arc::new(create_runtime()?);
        Self::open_with_runtime(path, rt)
    }

    pub fn open_with_runtime(path: &Path, rt: Arc<tokio::runtime::Runtime>) -> Result<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).with_context(|| {
                format!(
                    "creating SurrealDB graph substrate dir: {}",
                    parent.display()
                )
            })?;
        }
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
            path: Some(path.to_path_buf()),
            nodes: RwLock::new(BTreeMap::new()),
            edges: RwLock::new(SurrealEdgeIndexes::default()),
            node_row_hashes: RwLock::new(BTreeMap::new()),
            edge_row_hashes: RwLock::new(BTreeMap::new()),
        };
        if !store.try_load_sidecar()? {
            store.load_indexes()?;
            let _ = store.write_sidecar();
        }
        Ok(store)
    }

    pub fn in_memory() -> Result<Self> {
        let rt = Arc::new(create_runtime()?);
        Self::in_memory_with_runtime(rt)
    }

    pub fn in_memory_with_runtime(rt: Arc<tokio::runtime::Runtime>) -> Result<Self> {
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
            path: None,
            nodes: RwLock::new(BTreeMap::new()),
            edges: RwLock::new(SurrealEdgeIndexes::default()),
            node_row_hashes: RwLock::new(BTreeMap::new()),
            edge_row_hashes: RwLock::new(BTreeMap::new()),
        };
        store.load_indexes()?;
        Ok(store)
    }

    pub fn replace_projection_rows(&self, rows: &ConvexProjectionRows) -> Result<usize> {
        let nodes = rows.nodes.iter().map(node_from_row).collect::<Vec<_>>();
        let edges = rows.edges.iter().map(edge_from_row).collect::<Vec<_>>();
        self.replace_surreal_records(&nodes, &edges)?;
        let node_hashes: BTreeMap<String, String> = nodes
            .iter()
            .map(|n| (n.id.clone(), row_hash(n).unwrap_or_default()))
            .collect();
        let edge_hashes: BTreeMap<String, String> = edges
            .iter()
            .map(|e| (graph_edge_id(e), row_hash(e).unwrap_or_default()))
            .collect();
        self.replace_memory_indexes(nodes, edges)?;
        self.update_row_hashes(&node_hashes, &edge_hashes)?;
        Ok(rows.nodes.len() + rows.edges.len())
    }

    pub fn replace_projection_rows_delta(
        &self,
        rows: &ConvexProjectionRows,
    ) -> Result<DeltaRefreshStats> {
        let new_nodes: Vec<GraphNode> = rows.nodes.iter().map(node_from_row).collect();
        let new_edges: Vec<GraphEdge> = rows.edges.iter().map(edge_from_row).collect();

        let new_node_hashes: BTreeMap<String, String> = new_nodes
            .iter()
            .map(|n| (n.id.clone(), row_hash(n).unwrap_or_default()))
            .collect();
        let new_edge_hashes: BTreeMap<String, String> = new_edges
            .iter()
            .map(|e| (graph_edge_id(e), row_hash(e).unwrap_or_default()))
            .collect();

        let existing_node_ids: BTreeSet<String> =
            self.nodes_read()?.keys().cloned().collect();
        let existing_edge_keys: BTreeSet<String> = {
            let edges = self.edges_read()?;
            edges.all_edge_keys()
        };

        let stored_node_hashes = self.node_row_hashes.read().map_err(|_| anyhow!("lock"))?;
        let stored_edge_hashes = self.edge_row_hashes.read().map_err(|_| anyhow!("lock"))?;

        let changed_node_ids: Vec<String> = new_node_hashes
            .iter()
            .filter(|(id, hash)| {
                !existing_node_ids.contains(*id)
                    || (stored_node_hashes.get(*id) != Some(*hash))
            })
            .map(|(id, _)| id.clone())
            .collect();
        let changed_edge_keys: Vec<String> = new_edge_hashes
            .iter()
            .filter(|(key, hash)| {
                !existing_edge_keys.contains(*key)
                    || (stored_edge_hashes.get(*key) != Some(*hash))
            })
            .map(|(key, _)| key.clone())
            .collect();

        drop(stored_node_hashes);
        drop(stored_edge_hashes);

        let tombstoned_node_ids: Vec<String> = existing_node_ids
            .iter()
            .filter(|id| !new_node_hashes.contains_key(*id))
            .cloned()
            .collect();
        let tombstoned_edge_keys: Vec<String> = existing_edge_keys
            .iter()
            .filter(|key| !new_edge_hashes.contains_key(*key))
            .cloned()
            .collect();

        let unchanged_nodes = new_node_hashes.len() - changed_node_ids.len();
        let unchanged_edges = new_edge_hashes.len() - changed_edge_keys.len();

        let changed_nodes: Vec<&GraphNode> = new_nodes
            .iter()
            .filter(|n| changed_node_ids.contains(&n.id))
            .collect();
        let changed_edges: Vec<&GraphEdge> = new_edges
            .iter()
            .filter(|e| changed_edge_keys.contains(&graph_edge_id(e)))
            .collect();

        if !changed_nodes.is_empty()
            || !changed_edges.is_empty()
            || !tombstoned_node_ids.is_empty()
            || !tombstoned_edge_keys.is_empty()
        {
            self.delta_surreal_records(
                &changed_nodes,
                &changed_edges,
                &tombstoned_node_ids,
                &tombstoned_edge_keys,
            )?;
            self.delta_memory_indexes(
                &new_nodes,
                &new_edges,
                &tombstoned_node_ids,
                &tombstoned_edge_keys,
            )?;
            self.update_row_hashes(&new_node_hashes, &new_edge_hashes)?;
        }

        Ok(DeltaRefreshStats {
            unchanged_nodes,
            unchanged_edges,
            changed_nodes: changed_node_ids.len(),
            changed_edges: changed_edge_keys.len(),
            tombstoned_nodes: tombstoned_node_ids.len(),
            tombstoned_edges: tombstoned_edge_keys.len(),
        })
    }

    pub fn from_rows_file_backed(path: &Path, rows: &ConvexProjectionRows) -> Result<Self> {
        let store = Self::open(path)?;
        store.replace_projection_rows(rows)?;
        Ok(store)
    }

    pub fn open_or_refresh(
        path: &Path,
        rows: &ConvexProjectionRows,
    ) -> Result<(Self, WarmStartOutcome)> {
        let store = Self::open(path)?;
        let incoming_hash = row_hash(rows)?;
        let stored = store.stored_row_hash()?;
        if stored.as_deref() == Some(incoming_hash.as_str()) {
            return Ok((store, WarmStartOutcome::CacheHit));
        }
        store.replace_projection_rows_delta(rows)?;
        store.set_stored_row_hash(&incoming_hash)?;
        store.write_sidecar()?;
        Ok((store, WarmStartOutcome::Refreshed))
    }

    fn stored_row_hash(&self) -> Result<Option<String>> {
        block_on(&self.rt, async {
            let record: Option<MetadataRecord> = self
                .db
                .select((METADATA_TABLE, ROW_HASH_KEY))
                .await
                .context("reading SurrealDB stored row hash")?;
            Ok(record.map(|r| r.value))
        })
    }

    fn set_stored_row_hash(&self, hash: &str) -> Result<()> {
        block_on(&self.rt, async {
            let _: Option<MetadataRecord> = self
                .db
                .upsert((METADATA_TABLE, ROW_HASH_KEY))
                .content(MetadataRecord {
                    value: hash.to_string(),
                })
                .await
                .context("storing SurrealDB row hash")?;
            Ok(())
        })
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
        if let Some(ref store_path) = self.path {
            let _ = std::fs::remove_file(sidecar_path(store_path));
        }
        Ok(())
    }

    fn try_load_sidecar(&self) -> Result<bool> {
        let Some(ref store_path) = self.path else { return Ok(false) };
        let path = sidecar_path(store_path);
        if !path.exists() {
            return Ok(false);
        }
        let data = match std::fs::read(&path) {
            Ok(data) => data,
            Err(_) => return Ok(false),
        };
        let sidecar: SidecarData = match serde_json::from_slice(&data) {
            Ok(sidecar) => sidecar,
            Err(_) => return Ok(false),
        };
        if sidecar.version != SIDECAR_VERSION {
            return Ok(false);
        }
        let stored = self.stored_row_hash()?;
        if sidecar.stored_row_hash != stored {
            return Ok(false);
        }
        *self.nodes_write()? = sidecar.nodes;
        let mut edge_index = SurrealEdgeIndexes::default();
        for edge in sidecar.edges {
            edge_index.insert(edge);
        }
        *self.edges_write()? = edge_index;
        *self.node_row_hashes.write().map_err(|_| anyhow!("lock"))? = sidecar.node_row_hashes;
        *self.edge_row_hashes.write().map_err(|_| anyhow!("lock"))? = sidecar.edge_row_hashes;
        Ok(true)
    }

    fn write_sidecar(&self) -> Result<()> {
        let Some(ref store_path) = self.path else { return Ok(()) };
        let stored_hash = self.stored_row_hash()?;
        let Some(ref hash) = stored_hash else { return Ok(()) };
        let nodes = self.nodes_read()?;
        let edges = self.edges_read()?;
        let node_hashes = self.node_row_hashes.read().map_err(|_| anyhow!("lock"))?;
        let edge_hashes = self.edge_row_hashes.read().map_err(|_| anyhow!("lock"))?;
        let sidecar = SidecarData {
            version: SIDECAR_VERSION,
            stored_row_hash: Some(hash.clone()),
            nodes: nodes.clone(),
            edges: edges.ordered_edges(),
            node_row_hashes: node_hashes.clone(),
            edge_row_hashes: edge_hashes.clone(),
        };
        let data = serde_json::to_vec(&sidecar)?;
        let path = sidecar_path(store_path);
        std::fs::write(&path, data)
            .with_context(|| format!("writing SurrealDB sidecar {}", path.display()))?;
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
        let mut node_hashes = self.node_row_hashes.write().map_err(|_| anyhow!("lock"))?;
        node_hashes.clear();
        for record in &nodes {
            let node = GraphNode::from(record.clone());
            if let Some(ref hash) = record.row_hash {
                node_hashes.insert(node.id.clone(), hash.clone());
            }
            node_index.insert(node.id.clone(), node);
        }
        drop(node_index);
        drop(node_hashes);

        let mut edge_index = self.edges_write()?;
        edge_index.clear();
        let mut edge_hashes = self.edge_row_hashes.write().map_err(|_| anyhow!("lock"))?;
        edge_hashes.clear();
        for record in &edges {
            let edge = GraphEdge::from(record.clone());
            let key = graph_edge_id(&edge);
            if let Some(ref hash) = record.row_hash {
                edge_hashes.insert(key.clone(), hash.clone());
            }
            edge_index.insert(edge);
        }
        Ok(())
    }

    fn replace_surreal_records(&self, nodes: &[GraphNode], edges: &[GraphEdge]) -> Result<()> {
        let node_records = nodes
            .iter()
            .map(SurrealNodeInsertRecord::from)
            .collect::<Vec<_>>();
        let edge_records = edges
            .iter()
            .map(SurrealEdgeInsertRecord::from)
            .collect::<Vec<_>>();
        block_on(&self.rt, async move {
            self.db
                .query(format!(
                    r#"
                    BEGIN TRANSACTION;
                    DELETE {EDGE_TABLE};
                    DELETE {NODE_TABLE};
                    INSERT $nodes;
                    INSERT $edges;
                    COMMIT TRANSACTION;
                    "#
                ))
                .bind(("nodes", node_records))
                .bind(("edges", edge_records))
                .await
                .context("bulk replacing SurrealDB graph projection rows")?
                .check()
                .context("checking SurrealDB graph projection bulk replace")?;
            Ok::<(), anyhow::Error>(())
        })
    }

    fn replace_memory_indexes(&self, nodes: Vec<GraphNode>, edges: Vec<GraphEdge>) -> Result<()> {
        let mut next_nodes = BTreeMap::new();
        for node in nodes {
            next_nodes.insert(node.id.clone(), node);
        }
        let mut next_edges = SurrealEdgeIndexes::default();
        for edge in edges {
            next_edges.insert(edge);
        }
        *self.nodes_write()? = next_nodes;
        *self.edges_write()? = next_edges;
        Ok(())
    }

    fn delta_surreal_records(
        &self,
        changed_nodes: &[&GraphNode],
        changed_edges: &[&GraphEdge],
        tombstoned_node_ids: &[String],
        tombstoned_edge_keys: &[String],
    ) -> Result<()> {
        let node_records: Vec<SurrealNodeInsertRecord> = changed_nodes
            .iter()
            .map(|n| SurrealNodeInsertRecord::from(*n))
            .collect();
        let edge_records: Vec<SurrealEdgeInsertRecord> = changed_edges
            .iter()
            .map(|e| SurrealEdgeInsertRecord::from(*e))
            .collect();
        let delete_node_ids: Vec<String> = tombstoned_node_ids
            .iter()
            .cloned()
            .chain(changed_nodes.iter().map(|n| n.id.clone()))
            .collect();
        let delete_edge_keys: Vec<String> = tombstoned_edge_keys
            .iter()
            .cloned()
            .chain(changed_edges.iter().map(|e| graph_edge_id(e)))
            .collect();
        block_on(&self.rt, async move {
            self.db
                .query(format!(
                    r#"
                    BEGIN TRANSACTION;
                    DELETE {NODE_TABLE} WHERE external_id IN $delete_node_ids;
                    DELETE {EDGE_TABLE} WHERE edge_key IN $delete_edge_keys;
                    INSERT $nodes;
                    INSERT $edges;
                    COMMIT TRANSACTION;
                    "#
                ))
                .bind(("delete_node_ids", delete_node_ids))
                .bind(("delete_edge_keys", delete_edge_keys))
                .bind(("nodes", node_records))
                .bind(("edges", edge_records))
                .await
                .context("delta replacing SurrealDB graph projection rows")?
                .check()
                .context("checking SurrealDB graph projection delta replace")?;
            Ok::<(), anyhow::Error>(())
        })
    }

    fn delta_memory_indexes(
        &self,
        new_nodes: &[GraphNode],
        new_edges: &[GraphEdge],
        tombstoned_node_ids: &[String],
        tombstoned_edge_keys: &[String],
    ) -> Result<()> {
        {
            let mut node_index = self.nodes_write()?;
            for id in tombstoned_node_ids {
                node_index.remove(id);
            }
            for node in new_nodes {
                node_index.insert(node.id.clone(), node.clone());
            }
        }
        {
            let mut edge_index = self.edges_write()?;
            for key in tombstoned_edge_keys {
                edge_index.remove(key);
            }
            for edge in new_edges {
                edge_index.insert(edge.clone());
            }
        }
        Ok(())
    }

    fn update_row_hashes(
        &self,
        node_hashes: &BTreeMap<String, String>,
        edge_hashes: &BTreeMap<String, String>,
    ) -> Result<()> {
        *self.node_row_hashes.write().map_err(|_| anyhow!("lock"))? = node_hashes.clone();
        *self.edge_row_hashes.write().map_err(|_| anyhow!("lock"))? = edge_hashes.clone();
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
    use tsift_core::{ConvexNodeRow, GraphEdge, GraphNode, GraphPropertyFilter, GraphQueryOptions};

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
        let store_path = dir.path().join("surrealdb");
        let store =
            SurrealdbGraphStore::from_rows_file_backed(&store_path, &sample_rows()).unwrap();
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
        drop(store);

        let reopened = SurrealdbGraphStore::open(&store_path).unwrap();
        assert_eq!(reopened.graph_counts().unwrap(), (2, 1));
        assert_eq!(
            reopened
                .edge(&stable_graph_edge_id("node:a", "node:b", "calls"))
                .unwrap()
                .unwrap()
                .to_id,
            "node:b"
        );
        assert_eq!(
            reopened.delete_edge("node:a", "node:b", "calls").unwrap(),
            1
        );
        assert_eq!(reopened.graph_counts().unwrap(), (2, 0));
    }

    #[test]
    fn surrealdb_store_open_or_refresh_cache_hit_skips_replace() {
        let dir = tempfile::tempdir().unwrap();
        let store_path = dir.path().join("surrealdb");

        let (store1, outcome1) =
            SurrealdbGraphStore::open_or_refresh(&store_path, &sample_rows()).unwrap();
        assert_eq!(outcome1, WarmStartOutcome::Refreshed);
        assert_eq!(store1.graph_counts().unwrap(), (2, 1));
        drop(store1);

        let (store2, outcome2) =
            SurrealdbGraphStore::open_or_refresh(&store_path, &sample_rows()).unwrap();
        assert_eq!(outcome2, WarmStartOutcome::CacheHit);
        assert_eq!(store2.graph_counts().unwrap(), (2, 1));

        assert_eq!(
            store2
                .edge(&stable_graph_edge_id("node:a", "node:b", "calls"))
                .unwrap()
                .unwrap()
                .to_id,
            "node:b"
        );
    }

    #[test]
    fn surrealdb_store_open_or_refresh_detects_changed_rows() {
        let dir = tempfile::tempdir().unwrap();
        let store_path = dir.path().join("surrealdb");

        let (store1, outcome1) =
            SurrealdbGraphStore::open_or_refresh(&store_path, &sample_rows()).unwrap();
        assert_eq!(outcome1, WarmStartOutcome::Refreshed);
        drop(store1);

        let changed_rows = ConvexProjectionRows {
            nodes: vec![ConvexNodeRow::from(&GraphNode::new(
                "node:x",
                "file",
                "new.rs",
            ))],
            edges: vec![],
        };
        let (store2, outcome2) =
            SurrealdbGraphStore::open_or_refresh(&store_path, &changed_rows).unwrap();
        assert_eq!(outcome2, WarmStartOutcome::Refreshed);
        assert_eq!(store2.graph_counts().unwrap(), (1, 0));
        assert_eq!(store2.nodes_by_kind("file").unwrap().len(), 1);
    }

    #[test]
    fn surrealdb_store_delta_refresh_skips_unchanged_rows() {
        let store = SurrealdbGraphStore::in_memory().unwrap();
        let rows = sample_rows();
        let full = store.replace_projection_rows(&rows).unwrap();
        assert_eq!(full, 3);

        let stats = store.replace_projection_rows_delta(&rows).unwrap();
        assert_eq!(stats.unchanged_nodes, 2);
        assert_eq!(stats.unchanged_edges, 1);
        assert_eq!(stats.changed_nodes, 0);
        assert_eq!(stats.changed_edges, 0);
        assert_eq!(stats.tombstoned_nodes, 0);
        assert_eq!(stats.tombstoned_edges, 0);
        assert_eq!(store.graph_counts().unwrap(), (2, 1));
    }

    #[test]
    fn surrealdb_store_delta_refresh_detects_modified_rows() {
        let store = SurrealdbGraphStore::in_memory().unwrap();
        store.replace_projection_rows(&sample_rows()).unwrap();

        let modified_rows = ConvexProjectionRows {
            nodes: vec![
                ConvexNodeRow::from(&GraphNode::new("node:a", "symbol", "alpha").with_property("path", "a.rs")),
                ConvexNodeRow::from(
                    &GraphNode::new("node:b", "symbol", "beta-modified").with_property("path", "b-mod.rs"),
                ),
            ],
            edges: vec![ConvexEdgeRow::from(
                &GraphEdge::new("node:a", "node:b", "calls").with_property("kind", "indirect"),
            )],
        };
        let stats = store.replace_projection_rows_delta(&modified_rows).unwrap();
        assert_eq!(stats.unchanged_nodes, 1);
        assert_eq!(stats.changed_nodes, 1);
        assert_eq!(stats.changed_edges, 1);
        assert_eq!(stats.tombstoned_nodes, 0);
        assert_eq!(stats.tombstoned_edges, 0);

        let node_b = store.node("node:b").unwrap().unwrap();
        assert_eq!(node_b.label, "beta-modified");
        let edge = store
            .edge(&stable_graph_edge_id("node:a", "node:b", "calls"))
            .unwrap()
            .unwrap();
        assert_eq!(edge.properties.get("kind").unwrap(), "indirect");
    }

    #[test]
    fn surrealdb_store_delta_refresh_tombstones_removed_rows() {
        let store = SurrealdbGraphStore::in_memory().unwrap();
        store.replace_projection_rows(&sample_rows()).unwrap();

        let shrunk_rows = ConvexProjectionRows {
            nodes: vec![ConvexNodeRow::from(
                &GraphNode::new("node:a", "symbol", "alpha").with_property("path", "a.rs"),
            )],
            edges: vec![],
        };
        let stats = store.replace_projection_rows_delta(&shrunk_rows).unwrap();
        assert_eq!(stats.unchanged_nodes, 1);
        assert_eq!(stats.changed_nodes, 0);
        assert_eq!(stats.changed_edges, 0);
        assert_eq!(stats.tombstoned_nodes, 1);
        assert_eq!(stats.tombstoned_edges, 1);
        assert_eq!(store.graph_counts().unwrap(), (1, 0));
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
        let direct_surreal_query_edges = block_on(&store.rt, async {
            let mut response = store
                .db
                .query(format!(
                    "SELECT * FROM {EDGE_TABLE} WHERE from_external_id = $from_id AND kind = $kind"
                ))
                .bind(("from_id", "node:a"))
                .bind(("kind", "calls"))
                .await
                .unwrap();
            response.take::<Vec<SurrealEdgeRecord>>(0).unwrap()
        })
        .into_iter()
        .map(GraphEdge::from)
        .collect::<Vec<_>>();
        assert_eq!(direct_surreal_query_edges, vec![edge_ab.clone()]);
        assert_eq!(
            store.outgoing_edges("node:a", Some("calls")).unwrap(),
            direct_surreal_query_edges
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
        assert!(
            calls_page
                .page
                .diagnostics
                .iter()
                .any(|diagnostic| { diagnostic.contains("derived kind index") })
        );

        let incident_page = store
            .paged_incident_edges("node:a", Some("calls"), GraphQueryOptions::default())
            .unwrap();
        assert_eq!(incident_page.edges.len(), 2);
        assert!(
            incident_page
                .page
                .diagnostics
                .iter()
                .any(|diagnostic| { diagnostic.contains("Rust-side incoming/outgoing indexes") })
        );

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

    #[test]
    fn surrealdb_store_bulk_replace_resets_rows_and_indexes() {
        let store = SurrealdbGraphStore::in_memory().unwrap();
        store
            .upsert_node(&GraphNode::new("node:stale", "symbol", "stale"))
            .unwrap();
        store
            .upsert_edge(&GraphEdge::new("node:stale", "node:stale", "calls"))
            .unwrap();

        let node_a = GraphNode::new("node:a", "symbol", "alpha");
        let node_b = GraphNode::new("node:b", "symbol", "beta");
        let edge = GraphEdge::new("node:a", "node:b", "calls").with_property("batch", "yes");
        let rows = tsift_core::GraphProjection {
            nodes: vec![node_a, node_b],
            edges: vec![edge.clone()],
        }
        .to_convex_rows();

        assert_eq!(store.replace_projection_rows(&rows).unwrap(), 3);
        assert!(store.node("node:stale").unwrap().is_none());
        assert!(
            store
                .edge(&stable_graph_edge_id("node:stale", "node:stale", "calls"))
                .unwrap()
                .is_none()
        );
        assert_eq!(store.graph_counts().unwrap(), (2, 1));
        assert_eq!(
            store
                .paged_edges(None, GraphQueryOptions::default())
                .unwrap()
                .edges,
            vec![edge.clone()]
        );
        assert_eq!(store.delete_edge("node:a", "node:b", "calls").unwrap(), 1);
        assert_eq!(store.graph_counts().unwrap(), (2, 0));
    }

    #[test]
    fn surrealdb_store_multi_process_reader_writer_lock() {
        if std::env::var("TSIFT_SURREALDB_MULTI_PROC_ROLE").is_ok() {
            return;
        }

        let dir = tempfile::tempdir().unwrap();
        let store_path = dir.path().join("surrealdb");

        let rows = sample_rows();
        let store = SurrealdbGraphStore::from_rows_file_backed(&store_path, &rows).unwrap();
        assert_eq!(store.graph_counts().unwrap(), (2, 1));
        drop(store);

        let test_exe = std::env::current_exe().unwrap();
        let mut readers = Vec::new();
        for _ in 0..3 {
            let store_path_arg = store_path.to_str().unwrap().to_string();
            let child = std::process::Command::new(&test_exe)
                .env("TSIFT_SURREALDB_MULTI_PROC_ROLE", "reader")
                .env("TSIFT_SURREALDB_STORE_PATH", &store_path_arg)
                .args(["--test-threads=1", "surrealdb_store_multiproc_reader"])
                .stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::piped())
                .spawn()
                .expect("spawn reader process");
            readers.push(child);
        }

        let writer_path = store_path.to_str().unwrap().to_string();
        let writer = std::process::Command::new(&test_exe)
            .env("TSIFT_SURREALDB_MULTI_PROC_ROLE", "writer")
            .env("TSIFT_SURREALDB_STORE_PATH", &writer_path)
            .args(["--test-threads=1", "surrealdb_store_multiproc_writer"])
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .expect("spawn writer process");

        let writer_output = writer.wait_with_output().expect("writer wait");
        assert!(
            writer_output.status.success(),
            "writer process failed: {}",
            String::from_utf8_lossy(&writer_output.stderr)
        );

        for reader in readers {
            let output = reader.wait_with_output().expect("reader wait");
            assert!(
                output.status.success(),
                "reader process failed: {}",
                String::from_utf8_lossy(&output.stderr)
            );
        }

        let verify = SurrealdbGraphStore::open(&store_path).unwrap();
        let (nodes, edges) = verify.graph_counts().unwrap();
        assert_eq!(nodes, 3);
        assert_eq!(edges, 1);
        assert!(verify.node("node:c").unwrap().is_some());
    }

    #[test]
    fn surrealdb_store_multiproc_reader() {
        if std::env::var("TSIFT_SURREALDB_MULTI_PROC_ROLE").as_deref() != Ok("reader") {
            return;
        }
        let store_path = std::env::var("TSIFT_SURREALDB_STORE_PATH").unwrap();
        let store = SurrealdbGraphStore::open(Path::new(&store_path)).unwrap();
        let (nodes, edges) = store.graph_counts().unwrap();
        assert!(nodes >= 2);
        assert!(edges >= 1);
        let edge = store
            .edge(&stable_graph_edge_id("node:a", "node:b", "calls"))
            .unwrap()
            .unwrap();
        assert_eq!(edge.from_id, "node:a");
        assert_eq!(edge.to_id, "node:b");
    }

    #[test]
    fn surrealdb_store_multiproc_writer() {
        if std::env::var("TSIFT_SURREALDB_MULTI_PROC_ROLE").as_deref() != Ok("writer") {
            return;
        }
        let store_path = std::env::var("TSIFT_SURREALDB_STORE_PATH").unwrap();
        let store = SurrealdbGraphStore::open(Path::new(&store_path)).unwrap();
        store
            .upsert_node(&GraphNode::new("node:c", "symbol", "gamma"))
            .unwrap();
        let (nodes, edges) = store.graph_counts().unwrap();
        assert_eq!(nodes, 3);
        assert_eq!(edges, 1);
    }

    #[test]
    fn surrealdb_store_shared_runtime_across_stores() {
        let rt = Arc::new(
            tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()
                .unwrap(),
        );
        let store1 = SurrealdbGraphStore::in_memory_with_runtime(rt.clone()).unwrap();
        let store2 = SurrealdbGraphStore::in_memory_with_runtime(rt).unwrap();

        store1
            .upsert_node(&GraphNode::new("node:a", "symbol", "alpha"))
            .unwrap();
        store2
            .upsert_node(&GraphNode::new("node:b", "symbol", "beta"))
            .unwrap();

        assert_eq!(store1.graph_counts().unwrap().0, 1);
        assert_eq!(store2.graph_counts().unwrap().0, 1);
        assert!(store1.node("node:a").unwrap().is_some());
        assert!(store2.node("node:b").unwrap().is_some());
    }

    #[test]
    fn surrealdb_store_sidecar_skips_load_on_cache_hit() {
        let dir = tempfile::tempdir().unwrap();
        let store_path = dir.path().join("surrealdb");

        let (store1, outcome1) =
            SurrealdbGraphStore::open_or_refresh(&store_path, &sample_rows()).unwrap();
        assert_eq!(outcome1, WarmStartOutcome::Refreshed);
        assert_eq!(store1.graph_counts().unwrap(), (2, 1));
        assert!(sidecar_path(&store_path).exists());
        drop(store1);

        let (store2, outcome2) =
            SurrealdbGraphStore::open_or_refresh(&store_path, &sample_rows()).unwrap();
        assert_eq!(outcome2, WarmStartOutcome::CacheHit);
        assert_eq!(store2.graph_counts().unwrap(), (2, 1));
        assert_eq!(
            store2
                .edge(&stable_graph_edge_id("node:a", "node:b", "calls"))
                .unwrap()
                .unwrap()
                .to_id,
            "node:b"
        );
    }

    #[test]
    fn surrealdb_store_sidecar_invalidated_on_refresh() {
        let dir = tempfile::tempdir().unwrap();
        let store_path = dir.path().join("surrealdb");

        let (store1, _) =
            SurrealdbGraphStore::open_or_refresh(&store_path, &sample_rows()).unwrap();
        drop(store1);

        let changed_rows = ConvexProjectionRows {
            nodes: vec![
                ConvexNodeRow::from(&GraphNode::new("node:x", "file", "new.rs")),
                ConvexNodeRow::from(&GraphNode::new("node:y", "file", "other.rs")),
            ],
            edges: vec![ConvexEdgeRow::from(
                &GraphEdge::new("node:x", "node:y", "imports"),
            )],
        };
        let (store2, outcome2) =
            SurrealdbGraphStore::open_or_refresh(&store_path, &changed_rows).unwrap();
        assert_eq!(outcome2, WarmStartOutcome::Refreshed);
        assert_eq!(store2.graph_counts().unwrap(), (2, 1));
        assert_eq!(store2.nodes_by_kind("file").unwrap().len(), 2);
        drop(store2);

        let (store3, outcome3) =
            SurrealdbGraphStore::open_or_refresh(&store_path, &changed_rows).unwrap();
        assert_eq!(outcome3, WarmStartOutcome::CacheHit);
        assert_eq!(store3.graph_counts().unwrap(), (2, 1));
    }

    #[test]
    fn surrealdb_store_sidecar_corrupted_falls_back_to_load() {
        let dir = tempfile::tempdir().unwrap();
        let store_path = dir.path().join("surrealdb");

        let (store1, _) =
            SurrealdbGraphStore::open_or_refresh(&store_path, &sample_rows()).unwrap();
        drop(store1);

        let sidecar = sidecar_path(&store_path);
        assert!(sidecar.exists());
        std::fs::write(&sidecar, b"corrupted data").unwrap();

        let (store2, outcome2) =
            SurrealdbGraphStore::open_or_refresh(&store_path, &sample_rows()).unwrap();
        assert_eq!(outcome2, WarmStartOutcome::CacheHit);
        assert_eq!(store2.graph_counts().unwrap(), (2, 1));
    }

    #[test]
    fn surrealdb_store_sidecar_no_file_for_in_memory() {
        let store = SurrealdbGraphStore::in_memory().unwrap();
        store
            .upsert_node(&GraphNode::new("node:a", "symbol", "alpha"))
            .unwrap();
        assert!(store.path.is_none());
        assert!(store.write_sidecar().is_ok());
    }
}
