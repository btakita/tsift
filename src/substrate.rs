use anyhow::{Context, Result, bail};
use rusqlite::{Connection, OptionalExtension, Row, types::Type};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use std::collections::{BTreeMap, VecDeque};
use std::path::Path;

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

pub trait GraphStore {
    fn upsert_node(&self, node: &GraphNode) -> Result<()>;
    fn upsert_edge(&self, edge: &GraphEdge) -> Result<()>;
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
}

impl SqliteGraphStore {
    pub fn open(db_path: &Path) -> Result<Self> {
        if let Some(parent) = db_path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("creating graph substrate dir: {}", parent.display()))?;
        }
        let conn = Connection::open(db_path)
            .with_context(|| format!("opening graph substrate db: {}", db_path.display()))?;
        Self::from_connection(conn)
    }

    #[cfg(test)]
    pub fn in_memory() -> Result<Self> {
        Self::from_connection(Connection::open_in_memory()?)
    }

    fn from_connection(conn: Connection) -> Result<Self> {
        conn.pragma_update(None, "foreign_keys", "ON")?;
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
            "#,
        )?;
        Ok(Self { conn })
    }

    pub fn replace_projection(&mut self, projection: &GraphProjection) -> Result<()> {
        let tx = self.conn.transaction()?;
        tx.execute("DELETE FROM graph_edges", [])?;
        tx.execute("DELETE FROM graph_nodes", [])?;
        for node in &projection.nodes {
            tx.execute(
                r#"
                INSERT INTO graph_nodes
                    (id, kind, label, properties_json, provenance_json, freshness_json)
                VALUES (?1, ?2, ?3, ?4, ?5, ?6)
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
        }
        for edge in &projection.edges {
            tx.execute(
                r#"
                INSERT INTO graph_edges
                    (from_id, to_id, kind, properties_json, provenance_json, freshness_json)
                VALUES (?1, ?2, ?3, ?4, ?5, ?6)
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
        }
        tx.commit()?;
        Ok(())
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
}
