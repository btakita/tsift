use crate::index::{ReadOnlyRecovery, copy_read_only_snapshot, read_only_snapshot_recovery};
use anyhow::{Context, Result, bail};
use rusqlite::{Connection, OpenFlags, OptionalExtension, Row, types::Type};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use std::cell::RefCell;
use std::collections::{BTreeMap, VecDeque};
use std::path::{Path, PathBuf};
use std::time::Duration;

pub const SQLITE_GRAPH_SCHEMA_VERSION: i64 = 1;
const SQLITE_GRAPH_WAL_AUTOCHECKPOINT_PAGES: i64 = 256;

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
    pub fn new(
        from_id: impl Into<String>,
        to_id: impl Into<String>,
        kind: impl Into<String>,
    ) -> Self {
        Self {
            from_id: from_id.into(),
            to_id: to_id.into(),
            kind: kind.into(),
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
    fn sorted(mut self) -> Self {
        self.nodes.sort_by(|left, right| left.id.cmp(&right.id));
        self.edges.sort_by(|left, right| {
            left.from_id
                .cmp(&right.from_id)
                .then(left.kind.cmp(&right.kind))
                .then(left.to_id.cmp(&right.to_id))
        });
        self
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
    fn nodes_by_kind(&self, kind: &str) -> Result<Vec<GraphNode>>;
    fn outgoing_edges(&self, from_id: &str, kind: Option<&str>) -> Result<Vec<GraphEdge>>;
    fn shortest_path(
        &self,
        from_id: &str,
        to_id: &str,
        kind: Option<&str>,
    ) -> Result<Option<GraphPath>>;
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
            for edge in self.outgoing_edges(&current, kind)? {
                let edge_key = (edge.from_id.clone(), edge.kind.clone(), edge.to_id.clone());
                edges.entry(edge_key).or_insert_with(|| edge.clone());
                if !nodes.contains_key(&edge.to_id)
                    && let Some(node) = self.node(&edge.to_id)?
                {
                    nodes.insert(edge.to_id.clone(), node);
                    queue.push_back((edge.to_id.clone(), current_depth + 1));
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
        let raw = serde_json::json!([from_id, kind, to_id]).to_string();
        format!("edge:{}", blake3::hash(raw.as_bytes()).to_hex())
    }
}

impl From<&GraphEdge> for ConvexEdgeRow {
    fn from(edge: &GraphEdge) -> Self {
        Self {
            edge_key: ConvexEdgeRow::stable_key(&edge.from_id, &edge.to_id, &edge.kind),
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
                .then(left.to_id.cmp(&right.to_id))
                .then(left.kind.cmp(&right.kind))
        });
        Ok(edges)
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
        shortest_path_using_outgoing(from_id, to_id, kind, |current, kind| {
            self.outgoing_edges(current, kind)
        })
    }
}

pub struct SqliteGraphStore {
    conn: Connection,
    _snapshot_copy: Option<SnapshotCopyGuard>,
    read_only_recovery: Option<ReadOnlyRecovery>,
}

pub struct SqliteReadOnlyConnection {
    conn: Connection,
    _snapshot_copy: Option<SnapshotCopyGuard>,
    recovery: Option<ReadOnlyRecovery>,
}

impl SqliteReadOnlyConnection {
    pub fn conn(&self) -> &Connection {
        &self.conn
    }

    pub fn recovery(&self) -> Option<ReadOnlyRecovery> {
        self.recovery
    }
}

struct SnapshotCopyGuard {
    paths: Vec<PathBuf>,
}

impl Drop for SnapshotCopyGuard {
    fn drop(&mut self) {
        for path in &self.paths {
            let _ = std::fs::remove_file(path);
        }
    }
}

fn configure_writable_graph_connection(conn: &Connection, db_path: &Path) -> Result<()> {
    conn.busy_timeout(Duration::from_secs(5))?;
    conn.pragma_update(None, "journal_mode", "WAL")?;
    let mode: String = conn.query_row("PRAGMA journal_mode", [], |row| row.get(0))?;
    if mode.to_lowercase() != "wal" {
        bail!(
            "graph substrate db {} requires WAL mode for concurrent reads, got {}",
            db_path.display(),
            mode
        );
    }
    conn.pragma_update(
        None,
        "wal_autocheckpoint",
        SQLITE_GRAPH_WAL_AUTOCHECKPOINT_PAGES,
    )?;
    let checkpoint_pages: i64 =
        conn.query_row("PRAGMA wal_autocheckpoint", [], |row| row.get(0))?;
    if checkpoint_pages != SQLITE_GRAPH_WAL_AUTOCHECKPOINT_PAGES {
        bail!(
            "graph substrate db {} requires wal_autocheckpoint={}, got {}",
            db_path.display(),
            SQLITE_GRAPH_WAL_AUTOCHECKPOINT_PAGES,
            checkpoint_pages
        );
    }
    Ok(())
}

pub fn open_graph_read_only_connection(db_path: &Path) -> Result<SqliteReadOnlyConnection> {
    let conn = Connection::open_with_flags(
        db_path,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .with_context(|| format!("opening graph.db read-only: {}", db_path.display()))?;
    conn.busy_timeout(Duration::from_secs(5))?;
    Ok(SqliteReadOnlyConnection {
        conn,
        _snapshot_copy: None,
        recovery: None,
    })
}

pub fn open_graph_read_only_connection_resilient(
    db_path: &Path,
) -> Result<SqliteReadOnlyConnection> {
    match open_graph_read_only_connection(db_path).and_then(|connection| {
        connection
            .conn
            .query_row("SELECT COUNT(*) FROM sqlite_master", [], |_row| Ok(()))?;
        Ok(connection)
    }) {
        Ok(connection) => Ok(connection),
        Err(err) => match read_only_snapshot_recovery(db_path, &err) {
            Some(recovery) => open_graph_read_only_snapshot(db_path, recovery),
            None => Err(err),
        },
    }
}

fn open_graph_read_only_snapshot(
    db_path: &Path,
    recovery: ReadOnlyRecovery,
) -> Result<SqliteReadOnlyConnection> {
    let (snapshot_path, cleanup_paths) = copy_read_only_snapshot(db_path, "graph")?;
    let conn = Connection::open_with_flags(
        &snapshot_path,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .with_context(|| format!("opening graph.db snapshot {}", snapshot_path.display()))?;
    conn.busy_timeout(Duration::from_secs(5))?;
    Ok(SqliteReadOnlyConnection {
        conn,
        _snapshot_copy: Some(SnapshotCopyGuard {
            paths: cleanup_paths,
        }),
        recovery: Some(recovery),
    })
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SqliteProjectionRefresh {
    pub scope: String,
    pub projection_version: String,
    pub source_watermark: Option<String>,
    pub tombstoned_nodes: Vec<String>,
    pub tombstoned_edges: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SqliteProjectionVersion {
    pub projection_version: String,
    pub content_hash: Option<String>,
    pub source_watermark: Option<String>,
}

impl SqliteGraphStore {
    pub fn open(db_path: &Path) -> Result<Self> {
        if let Some(parent) = db_path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("creating graph substrate dir: {}", parent.display()))?;
        }
        let conn = Connection::open(db_path)
            .with_context(|| format!("opening graph substrate db: {}", db_path.display()))?;
        configure_writable_graph_connection(&conn, db_path)?;
        Self::from_connection(conn)
    }

    pub fn in_memory() -> Result<Self> {
        let conn = Connection::open_in_memory()?;
        conn.busy_timeout(Duration::from_secs(5))?;
        Self::from_connection(conn)
    }

    pub fn open_read_only(db_path: &Path) -> Result<Self> {
        let connection = open_graph_read_only_connection(db_path)?;
        Self::from_read_only_connection(connection)
    }

    pub fn open_read_only_resilient(db_path: &Path) -> Result<Self> {
        let connection = open_graph_read_only_connection_resilient(db_path)?;
        Self::from_read_only_connection(connection)
    }

    pub fn read_only_recovery(&self) -> Option<ReadOnlyRecovery> {
        self.read_only_recovery
    }

    fn from_connection(conn: Connection) -> Result<Self> {
        conn.pragma_update(None, "foreign_keys", "ON")?;
        let user_version: i64 =
            conn.pragma_query_value(None, "user_version", |row| row.get::<_, i64>(0))?;
        if user_version > SQLITE_GRAPH_SCHEMA_VERSION {
            bail!(
                "graph.db schema version {} is newer than supported version {}",
                user_version,
                SQLITE_GRAPH_SCHEMA_VERSION
            );
        }
        conn.execute_batch(
            r#"
            CREATE TABLE IF NOT EXISTS graph_nodes (
                id TEXT PRIMARY KEY,
                kind TEXT NOT NULL,
                label TEXT NOT NULL,
                properties_json TEXT NOT NULL DEFAULT '{}',
                provenance_json TEXT NOT NULL DEFAULT '[]',
                freshness_json TEXT
            );
            CREATE INDEX IF NOT EXISTS idx_graph_nodes_kind
                ON graph_nodes(kind);

            CREATE TABLE IF NOT EXISTS graph_edges (
                from_id TEXT NOT NULL,
                to_id TEXT NOT NULL,
                kind TEXT NOT NULL,
                properties_json TEXT NOT NULL DEFAULT '{}',
                provenance_json TEXT NOT NULL DEFAULT '[]',
                freshness_json TEXT,
                PRIMARY KEY (from_id, to_id, kind),
                FOREIGN KEY (from_id) REFERENCES graph_nodes(id) ON DELETE CASCADE,
                FOREIGN KEY (to_id) REFERENCES graph_nodes(id) ON DELETE CASCADE
            );
            CREATE INDEX IF NOT EXISTS idx_graph_edges_from_kind
                ON graph_edges(from_id, kind);
            CREATE INDEX IF NOT EXISTS idx_graph_edges_to_kind
                ON graph_edges(to_id, kind);

            CREATE TABLE IF NOT EXISTS graph_projection_versions (
                scope TEXT PRIMARY KEY,
                projection_version TEXT NOT NULL,
                content_hash TEXT,
                source_watermark TEXT,
                observed_at_unix INTEGER NOT NULL
            );

            CREATE TABLE IF NOT EXISTS graph_tombstones (
                row_key TEXT PRIMARY KEY,
                row_kind TEXT NOT NULL,
                deleted_at_unix INTEGER NOT NULL
            );
            "#,
        )?;
        if user_version < SQLITE_GRAPH_SCHEMA_VERSION {
            conn.pragma_update(None, "user_version", SQLITE_GRAPH_SCHEMA_VERSION)?;
        }
        Ok(Self {
            conn,
            _snapshot_copy: None,
            read_only_recovery: None,
        })
    }

    fn from_read_only_connection(connection: SqliteReadOnlyConnection) -> Result<Self> {
        connection.conn.pragma_update(None, "foreign_keys", "ON")?;
        let user_version: i64 =
            connection
                .conn
                .pragma_query_value(None, "user_version", |row| row.get::<_, i64>(0))?;
        if user_version > SQLITE_GRAPH_SCHEMA_VERSION {
            bail!(
                "graph.db schema version {} is newer than supported version {}",
                user_version,
                SQLITE_GRAPH_SCHEMA_VERSION
            );
        }
        connection
            .conn
            .query_row("SELECT COUNT(*) FROM sqlite_master", [], |_row| Ok(()))?;
        Ok(Self {
            conn: connection.conn,
            _snapshot_copy: connection._snapshot_copy,
            read_only_recovery: connection.recovery,
        })
    }

    pub fn replace_projection(&mut self, projection: &GraphProjection) -> Result<()> {
        self.replace_projection_with_version("root", projection, None, None)
            .map(|_| ())
    }

    pub fn replace_projection_with_version(
        &mut self,
        scope: impl Into<String>,
        projection: &GraphProjection,
        projection_version: Option<&str>,
        source_watermark: Option<String>,
    ) -> Result<SqliteProjectionRefresh> {
        let scope = scope.into();
        let projection_version = projection_version
            .map(str::to_string)
            .or_else(|| projection_version_from_nodes(&projection.nodes))
            .unwrap_or_else(|| "unversioned".to_string());
        let projection_hash = projection_hash_from_nodes(&projection.nodes);
        let new_nodes = projection
            .nodes
            .iter()
            .map(|node| node.id.as_str())
            .collect::<std::collections::BTreeSet<_>>();
        let new_edges = projection
            .edges
            .iter()
            .map(|edge| ConvexEdgeRow::stable_key(&edge.from_id, &edge.to_id, &edge.kind))
            .collect::<std::collections::BTreeSet<_>>();
        let existing_nodes = self.existing_node_ids()?;
        let existing_edges = self.existing_edge_keys()?;
        let tombstoned_nodes = existing_nodes
            .into_iter()
            .filter(|id| !new_nodes.contains(id.as_str()))
            .collect::<Vec<_>>();
        let tombstoned_edges = existing_edges
            .into_iter()
            .filter(|key| !new_edges.contains(key.as_str()))
            .collect::<Vec<_>>();
        let observed_at_unix = unix_now();

        let tx = self.conn.transaction()?;
        tx.execute("DELETE FROM graph_edges", [])?;
        tx.execute("DELETE FROM graph_nodes", [])?;
        {
            let mut insert_node = tx.prepare(
                r#"
                INSERT INTO graph_nodes
                    (id, kind, label, properties_json, provenance_json, freshness_json)
                VALUES (?1, ?2, ?3, ?4, ?5, ?6)
                "#,
            )?;
            for node in &projection.nodes {
                insert_node.execute((
                    &node.id,
                    &node.kind,
                    &node.label,
                    to_json(&node.properties)?,
                    to_json(&node.provenance)?,
                    optional_to_json(&node.freshness)?,
                ))?;
            }
        }
        {
            let mut insert_edge = tx.prepare(
                r#"
                INSERT INTO graph_edges
                    (from_id, to_id, kind, properties_json, provenance_json, freshness_json)
                VALUES (?1, ?2, ?3, ?4, ?5, ?6)
                "#,
            )?;
            for edge in &projection.edges {
                insert_edge.execute((
                    &edge.from_id,
                    &edge.to_id,
                    &edge.kind,
                    to_json(&edge.properties)?,
                    to_json(&edge.provenance)?,
                    optional_to_json(&edge.freshness)?,
                ))?;
            }
        }
        tx.execute(
            r#"
            INSERT INTO graph_projection_versions
                (scope, projection_version, content_hash, source_watermark, observed_at_unix)
            VALUES (?1, ?2, ?3, ?4, ?5)
            ON CONFLICT(scope) DO UPDATE SET
                projection_version = excluded.projection_version,
                content_hash = excluded.content_hash,
                source_watermark = excluded.source_watermark,
                observed_at_unix = excluded.observed_at_unix
            "#,
            (
                &scope,
                &projection_version,
                &projection_hash,
                &source_watermark,
                observed_at_unix,
            ),
        )?;
        {
            let mut insert_node_tombstone = tx.prepare(
                r#"
                INSERT INTO graph_tombstones (row_key, row_kind, deleted_at_unix)
                VALUES (?1, 'node', ?2)
                ON CONFLICT(row_key) DO UPDATE SET
                    row_kind = excluded.row_kind,
                    deleted_at_unix = excluded.deleted_at_unix
                "#,
            )?;
            for id in &tombstoned_nodes {
                insert_node_tombstone.execute((format!("node:{id}"), observed_at_unix))?;
            }
        }
        {
            let mut insert_edge_tombstone = tx.prepare(
                r#"
                INSERT INTO graph_tombstones (row_key, row_kind, deleted_at_unix)
                VALUES (?1, 'edge', ?2)
                ON CONFLICT(row_key) DO UPDATE SET
                    row_kind = excluded.row_kind,
                    deleted_at_unix = excluded.deleted_at_unix
                "#,
            )?;
            for key in &tombstoned_edges {
                insert_edge_tombstone.execute((format!("edge:{key}"), observed_at_unix))?;
            }
        }
        tx.commit()?;
        Ok(SqliteProjectionRefresh {
            scope,
            projection_version,
            source_watermark,
            tombstoned_nodes,
            tombstoned_edges,
        })
    }

    pub fn projection_version(&self, scope: &str) -> Result<Option<SqliteProjectionVersion>> {
        self.conn
            .query_row(
                r#"
                SELECT projection_version, content_hash, source_watermark
                FROM graph_projection_versions
                WHERE scope = ?1
                "#,
                [scope],
                |row| {
                    Ok(SqliteProjectionVersion {
                        projection_version: row.get(0)?,
                        content_hash: row.get(1)?,
                        source_watermark: row.get(2)?,
                    })
                },
            )
            .optional()
            .map_err(Into::into)
    }

    fn existing_node_ids(&self) -> Result<Vec<String>> {
        let mut stmt = self
            .conn
            .prepare("SELECT id FROM graph_nodes ORDER BY id")?;
        collect_rows(stmt.query_map([], |row| row.get(0))?)
    }

    fn existing_edge_keys(&self) -> Result<Vec<String>> {
        let mut stmt = self.conn.prepare(
            "SELECT from_id, to_id, kind FROM graph_edges ORDER BY from_id, kind, to_id",
        )?;
        collect_rows(stmt.query_map([], |row| {
            let from_id: String = row.get(0)?;
            let to_id: String = row.get(1)?;
            let kind: String = row.get(2)?;
            Ok(ConvexEdgeRow::stable_key(&from_id, &to_id, &kind))
        })?)
    }
}

impl GraphStore for SqliteGraphStore {
    fn upsert_node(&self, node: &GraphNode) -> Result<()> {
        self.conn.execute(
            r#"
            INSERT INTO graph_nodes
                (id, kind, label, properties_json, provenance_json, freshness_json)
            VALUES (?1, ?2, ?3, ?4, ?5, ?6)
            ON CONFLICT(id) DO UPDATE SET
                kind = excluded.kind,
                label = excluded.label,
                properties_json = excluded.properties_json,
                provenance_json = excluded.provenance_json,
                freshness_json = excluded.freshness_json
            "#,
            (
                &node.id,
                &node.kind,
                &node.label,
                to_json(&node.properties)?,
                to_json(&node.provenance)?,
                optional_to_json(&node.freshness)?,
            ),
        )?;
        Ok(())
    }

    fn upsert_edge(&self, edge: &GraphEdge) -> Result<()> {
        self.conn.execute(
            r#"
            INSERT INTO graph_edges
                (from_id, to_id, kind, properties_json, provenance_json, freshness_json)
            VALUES (?1, ?2, ?3, ?4, ?5, ?6)
            ON CONFLICT(from_id, to_id, kind) DO UPDATE SET
                properties_json = excluded.properties_json,
                provenance_json = excluded.provenance_json,
                freshness_json = excluded.freshness_json
            "#,
            (
                &edge.from_id,
                &edge.to_id,
                &edge.kind,
                to_json(&edge.properties)?,
                to_json(&edge.provenance)?,
                optional_to_json(&edge.freshness)?,
            ),
        )?;
        Ok(())
    }

    fn delete_node(&self, id: &str) -> Result<usize> {
        self.conn
            .execute("DELETE FROM graph_nodes WHERE id = ?1", [id])
            .map_err(Into::into)
    }

    fn delete_edge(&self, from_id: &str, to_id: &str, kind: &str) -> Result<usize> {
        self.conn
            .execute(
                "DELETE FROM graph_edges WHERE from_id = ?1 AND to_id = ?2 AND kind = ?3",
                (from_id, to_id, kind),
            )
            .map_err(Into::into)
    }

    fn node(&self, id: &str) -> Result<Option<GraphNode>> {
        self.conn
            .query_row(
                r#"
                SELECT id, kind, label, properties_json, provenance_json, freshness_json
                FROM graph_nodes
                WHERE id = ?1
                "#,
                [id],
                node_from_row,
            )
            .optional()
            .map_err(Into::into)
    }

    fn all_nodes(&self) -> Result<Vec<GraphNode>> {
        let mut stmt = self.conn.prepare(
            r#"
            SELECT id, kind, label, properties_json, provenance_json, freshness_json
            FROM graph_nodes
            ORDER BY id
            "#,
        )?;
        collect_rows(stmt.query_map([], node_from_row)?)
    }

    fn all_edges(&self) -> Result<Vec<GraphEdge>> {
        let mut stmt = self.conn.prepare(
            r#"
            SELECT from_id, to_id, kind, properties_json, provenance_json, freshness_json
            FROM graph_edges
            ORDER BY from_id, to_id, kind
            "#,
        )?;
        collect_rows(stmt.query_map([], edge_from_row)?)
    }

    fn nodes_by_kind(&self, kind: &str) -> Result<Vec<GraphNode>> {
        let mut stmt = self.conn.prepare(
            r#"
            SELECT id, kind, label, properties_json, provenance_json, freshness_json
            FROM graph_nodes
            WHERE kind = ?1
            ORDER BY id
            "#,
        )?;
        collect_rows(stmt.query_map([kind], node_from_row)?)
    }

    fn outgoing_edges(&self, from_id: &str, kind: Option<&str>) -> Result<Vec<GraphEdge>> {
        match kind {
            Some(kind) => {
                let mut stmt = self.conn.prepare(
                    r#"
                    SELECT from_id, to_id, kind, properties_json, provenance_json, freshness_json
                    FROM graph_edges
                    WHERE from_id = ?1 AND kind = ?2
                    ORDER BY to_id, kind
                    "#,
                )?;
                collect_rows(stmt.query_map((from_id, kind), edge_from_row)?)
            }
            None => {
                let mut stmt = self.conn.prepare(
                    r#"
                    SELECT from_id, to_id, kind, properties_json, provenance_json, freshness_json
                    FROM graph_edges
                    WHERE from_id = ?1
                    ORDER BY to_id, kind
                    "#,
                )?;
                collect_rows(stmt.query_map([from_id], edge_from_row)?)
            }
        }
    }

    fn shortest_path(
        &self,
        from_id: &str,
        to_id: &str,
        kind: Option<&str>,
    ) -> Result<Option<GraphPath>> {
        shortest_path_using_outgoing(from_id, to_id, kind, |current, kind| {
            self.outgoing_edges(current, kind)
        })
    }
}

fn shortest_path_using_outgoing(
    from_id: &str,
    to_id: &str,
    kind: Option<&str>,
    mut outgoing_edges: impl FnMut(&str, Option<&str>) -> Result<Vec<GraphEdge>>,
) -> Result<Option<GraphPath>> {
    if from_id == to_id {
        return Ok(Some(GraphPath {
            nodes: vec![from_id.to_string()],
            hops: 0,
        }));
    }

    let mut queue = VecDeque::new();
    let mut parent = BTreeMap::<String, String>::new();
    parent.insert(from_id.to_string(), String::new());
    queue.push_back(from_id.to_string());

    while let Some(current) = queue.pop_front() {
        for edge in outgoing_edges(&current, kind)? {
            if parent.contains_key(&edge.to_id) {
                continue;
            }
            parent.insert(edge.to_id.clone(), current.clone());
            if edge.to_id == to_id {
                let mut nodes = vec![to_id.to_string()];
                let mut cursor = to_id;
                while let Some(previous) = parent.get(cursor) {
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

fn to_json<T: Serialize>(value: &T) -> Result<String> {
    serde_json::to_string(value).map_err(Into::into)
}

fn optional_to_json<T: Serialize>(value: &Option<T>) -> Result<Option<String>> {
    value.as_ref().map(to_json).transpose()
}

fn collect_rows<T>(
    rows: impl Iterator<Item = std::result::Result<T, rusqlite::Error>>,
) -> Result<Vec<T>> {
    rows.collect::<std::result::Result<Vec<_>, _>>()
        .map_err(Into::into)
}

fn node_from_row(row: &Row<'_>) -> rusqlite::Result<GraphNode> {
    let properties_json: String = row.get(3)?;
    let provenance_json: String = row.get(4)?;
    let freshness_json: Option<String> = row.get(5)?;
    Ok(GraphNode {
        id: row.get(0)?,
        kind: row.get(1)?,
        label: row.get(2)?,
        properties: from_json(3, &properties_json)?,
        provenance: from_json(4, &provenance_json)?,
        freshness: optional_from_json(5, freshness_json)?,
    })
}

fn edge_from_row(row: &Row<'_>) -> rusqlite::Result<GraphEdge> {
    let properties_json: String = row.get(3)?;
    let provenance_json: String = row.get(4)?;
    let freshness_json: Option<String> = row.get(5)?;
    Ok(GraphEdge {
        from_id: row.get(0)?,
        to_id: row.get(1)?,
        kind: row.get(2)?,
        properties: from_json(3, &properties_json)?,
        provenance: from_json(4, &provenance_json)?,
        freshness: optional_from_json(5, freshness_json)?,
    })
}

fn from_json<T: DeserializeOwned>(column: usize, raw: &str) -> rusqlite::Result<T> {
    serde_json::from_str(raw)
        .map_err(|err| rusqlite::Error::FromSqlConversionFailure(column, Type::Text, Box::new(err)))
}

fn optional_from_json<T: DeserializeOwned>(
    column: usize,
    raw: Option<String>,
) -> rusqlite::Result<Option<T>> {
    raw.map(|value| from_json(column, &value)).transpose()
}

fn projection_version_from_nodes(nodes: &[GraphNode]) -> Option<String> {
    nodes
        .iter()
        .find(|node| node.kind == "projection_meta")
        .and_then(|node| node.properties.get("projection_version").cloned())
}

fn projection_hash_from_nodes(nodes: &[GraphNode]) -> Option<String> {
    nodes
        .iter()
        .find(|node| node.kind == "projection_meta")
        .and_then(|node| node.properties.get("content_hash").cloned())
}

fn unix_now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_secs() as i64)
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;

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
    fn sqlite_store_round_trips_generic_nodes_edges() {
        let store = SqliteGraphStore::in_memory().unwrap();
        let source = sample_provenance();
        let node = GraphNode::new("doc:livekit", "document", "LiveKit guide")
            .with_property("domain", "livekit")
            .with_provenance(source.clone())
            .with_freshness(GraphFreshness::content_hash("node-hash"));
        let topic = GraphNode::new("topic:rooms", "topic", "Rooms");
        let edge = GraphEdge::new("doc:livekit", "topic:rooms", "mentions")
            .with_property("confidence", "0.91")
            .with_provenance(source)
            .with_freshness(GraphFreshness::content_hash("edge-hash"));

        store.upsert_node(&node).unwrap();
        store.upsert_node(&topic).unwrap();
        store.upsert_edge(&edge).unwrap();

        assert_eq!(store.node("doc:livekit").unwrap(), Some(node));
        assert_eq!(store.nodes_by_kind("topic").unwrap(), vec![topic]);
        assert_eq!(store.all_nodes().unwrap().len(), 2);
        assert_eq!(store.all_edges().unwrap().len(), 1);
        assert_eq!(
            store
                .outgoing_edges("doc:livekit", Some("mentions"))
                .unwrap(),
            vec![edge]
        );
    }

    #[test]
    fn graph_projection_round_trips_through_backend_agnostic_store_contract() {
        let sqlite = SqliteGraphStore::in_memory().unwrap();
        assert_projection_store_contract(&sqlite);

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

        assert_crud_contract(&SqliteGraphStore::in_memory().unwrap());
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
    fn sqlite_store_filters_edges_by_kind_and_paths() {
        let store = SqliteGraphStore::in_memory().unwrap();
        for id in ["a", "b", "c"] {
            store
                .upsert_node(&GraphNode::new(id, "symbol", id))
                .unwrap();
        }
        store
            .upsert_edge(&GraphEdge::new("a", "b", "calls"))
            .unwrap();
        store
            .upsert_edge(&GraphEdge::new("a", "c", "documents"))
            .unwrap();
        store
            .upsert_edge(&GraphEdge::new("b", "c", "calls"))
            .unwrap();

        let calls = store.outgoing_edges("a", Some("calls")).unwrap();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].to_id, "b");

        let path = store
            .shortest_path("a", "c", Some("calls"))
            .unwrap()
            .unwrap();
        assert_eq!(path.nodes, vec!["a", "b", "c"]);
        assert_eq!(path.hops, 2);

        assert!(
            store
                .shortest_path("c", "a", Some("calls"))
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn sqlite_projection_refresh_tracks_versions_watermarks_and_tombstones() {
        let mut store = SqliteGraphStore::in_memory().unwrap();
        let mut projection = sample_projection();
        projection.nodes.push(
            GraphNode::new(
                "projection:fixture",
                "projection_meta",
                "fixture projection",
            )
            .with_property("projection_version", "fixture-v1")
            .with_property("content_hash", "hash-a"),
        );
        store
            .replace_projection_with_version(
                "root",
                &projection,
                Some("fixture-v1"),
                Some("commit-a".to_string()),
            )
            .unwrap();

        projection.nodes.retain(|node| node.id != "topic:egress");
        projection.edges.retain(|edge| edge.to_id != "topic:egress");
        let refresh = store
            .replace_projection_with_version(
                "root",
                &projection,
                Some("fixture-v2"),
                Some("commit-b".to_string()),
            )
            .unwrap();

        assert_eq!(refresh.projection_version, "fixture-v2");
        assert_eq!(refresh.source_watermark.as_deref(), Some("commit-b"));
        assert_eq!(refresh.tombstoned_nodes, vec!["topic:egress".to_string()]);
        assert_eq!(refresh.tombstoned_edges.len(), 1);
        let version = store.projection_version("root").unwrap().unwrap();
        assert_eq!(version.projection_version, "fixture-v2");
        assert_eq!(version.source_watermark.as_deref(), Some("commit-b"));
    }

    #[test]
    fn sqlite_projection_refresh_handles_bulk_row_replacement() {
        let mut store = SqliteGraphStore::in_memory().unwrap();
        let source = GraphProvenance::new("fixture", "bulk");
        let mut projection = GraphProjection::default();
        for idx in 0..128 {
            projection.nodes.push(
                GraphNode::new(
                    format!("node:{idx:03}"),
                    if idx % 2 == 0 { "symbol" } else { "file" },
                    format!("bulk node {idx:03}"),
                )
                .with_property("ordinal", idx.to_string())
                .with_provenance(source.clone())
                .with_freshness(GraphFreshness::content_hash(format!("node-hash-{idx:03}"))),
            );
        }
        for idx in 0..127 {
            projection.edges.push(
                GraphEdge::new(
                    format!("node:{idx:03}"),
                    format!("node:{:03}", idx + 1),
                    "next",
                )
                .with_property("ordinal", idx.to_string())
                .with_provenance(source.clone())
                .with_freshness(GraphFreshness::content_hash(format!("edge-hash-{idx:03}"))),
            );
        }

        store
            .replace_projection_with_version(
                "root",
                &projection,
                Some("bulk-v1"),
                Some("commit-a".to_string()),
            )
            .unwrap();

        projection
            .nodes
            .retain(|node| !node.id.ends_with("000") && !node.id.ends_with("064"));
        projection.edges.retain(|edge| {
            !edge.from_id.ends_with("000")
                && !edge.to_id.ends_with("000")
                && !edge.from_id.ends_with("064")
                && !edge.to_id.ends_with("064")
        });
        let refresh = store
            .replace_projection_with_version(
                "root",
                &projection,
                Some("bulk-v2"),
                Some("commit-b".to_string()),
            )
            .unwrap();

        assert_eq!(store.all_nodes().unwrap().len(), 126);
        assert_eq!(store.all_edges().unwrap().len(), 124);
        assert_eq!(
            refresh.tombstoned_nodes,
            vec!["node:000".to_string(), "node:064".to_string()]
        );
        assert_eq!(refresh.tombstoned_edges.len(), 3);
        assert_eq!(
            store
                .projection_version("root")
                .unwrap()
                .unwrap()
                .source_watermark
                .as_deref(),
            Some("commit-b")
        );
    }
}
