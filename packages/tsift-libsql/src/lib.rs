use anyhow::{Context, Result};
use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;
use tsift_core::{
    GraphEdge, GraphNode, GraphPath, GraphStore, SQLITE_GRAPH_SCHEMA_VERSION,
};

fn block_on<F: std::future::Future>(rt: &tokio::runtime::Runtime, f: F) -> F::Output {
    rt.block_on(f)
}

pub struct LibsqlGraphStore {
    conn: libsql::Connection,
    rt: tokio::runtime::Runtime,
}

impl LibsqlGraphStore {
    pub fn open(db_path: &Path) -> Result<Self> {
        if let Some(parent) = db_path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("creating libsql graph substrate dir: {}", parent.display()))?;
        }
        let rt = tokio::runtime::Runtime::new().context("creating tokio runtime for libsql")?;
        let db = block_on(&rt, libsql::Builder::new_local(db_path).build())
            .with_context(|| format!("opening libsql graph substrate db: {}", db_path.display()))?;
        let conn = db.connect()
            .with_context(|| format!("connecting to libsql graph substrate db: {}", db_path.display()))?;
        let store = Self { conn, rt };
        store.init_schema()?;
        Ok(store)
    }

    pub fn open_remote(url: &str, auth_token: &str) -> Result<Self> {
        let rt = tokio::runtime::Runtime::new().context("creating tokio runtime for libsql remote")?;
        let db = block_on(&rt, libsql::Builder::new_remote(url.to_string(), auth_token.to_string()).build())
            .context("building libsql remote database")?;
        let conn = db.connect().context("connecting to libsql remote database")?;
        let store = Self { conn, rt };
        store.init_schema()?;
        Ok(store)
    }

    pub fn in_memory() -> Result<Self> {
        let rt = tokio::runtime::Runtime::new().context("creating tokio runtime for libsql")?;
        let db = block_on(&rt, libsql::Builder::new_local(":memory:").build())
            .context("opening in-memory libsql database")?;
        let conn = db.connect().context("connecting to in-memory libsql database")?;
        let store = Self { conn, rt };
        store.init_schema()?;
        Ok(store)
    }

    fn init_schema(&self) -> Result<()> {
        block_on(&self.rt, async {
            self.conn.execute_batch(&format!(
                r#"
                PRAGMA foreign_keys = ON;
                PRAGMA busy_timeout = 5000;

                CREATE TABLE IF NOT EXISTS graph_nodes (
                    id TEXT PRIMARY KEY,
                    kind TEXT NOT NULL,
                    label TEXT NOT NULL,
                    properties_json TEXT NOT NULL DEFAULT '{{}}',
                    provenance_json TEXT NOT NULL DEFAULT '[]',
                    freshness_json TEXT,
                    row_hash TEXT,
                    source_watermark TEXT
                );
                CREATE INDEX IF NOT EXISTS idx_graph_nodes_kind
                    ON graph_nodes(kind);
                CREATE INDEX IF NOT EXISTS idx_graph_nodes_kind_label
                    ON graph_nodes(kind, label, id);

                CREATE TABLE IF NOT EXISTS graph_edges (
                    edge_key TEXT NOT NULL UNIQUE,
                    from_id TEXT NOT NULL,
                    to_id TEXT NOT NULL,
                    kind TEXT NOT NULL,
                    properties_json TEXT NOT NULL DEFAULT '{{}}',
                    provenance_json TEXT NOT NULL DEFAULT '[]',
                    freshness_json TEXT,
                    row_hash TEXT,
                    source_watermark TEXT,
                    PRIMARY KEY (from_id, to_id, kind),
                    FOREIGN KEY (from_id) REFERENCES graph_nodes(id) ON DELETE CASCADE,
                    FOREIGN KEY (to_id) REFERENCES graph_nodes(id) ON DELETE CASCADE
                );
                CREATE INDEX IF NOT EXISTS idx_graph_edges_from_kind
                    ON graph_edges(from_id, kind);
                CREATE INDEX IF NOT EXISTS idx_graph_edges_to_kind
                    ON graph_edges(to_id, kind);

                CREATE TABLE IF NOT EXISTS graph_node_properties (
                    node_id TEXT NOT NULL,
                    key TEXT NOT NULL,
                    value TEXT NOT NULL,
                    PRIMARY KEY (node_id, key),
                    FOREIGN KEY (node_id) REFERENCES graph_nodes(id) ON DELETE CASCADE
                );
                CREATE INDEX IF NOT EXISTS idx_graph_node_properties_key_value_node
                    ON graph_node_properties(key, value, node_id);

                CREATE TABLE IF NOT EXISTS graph_edge_properties (
                    edge_key TEXT NOT NULL,
                    key TEXT NOT NULL,
                    value TEXT NOT NULL,
                    PRIMARY KEY (edge_key, key),
                    FOREIGN KEY (edge_key) REFERENCES graph_edges(edge_key) ON DELETE CASCADE
                );
                CREATE INDEX IF NOT EXISTS idx_graph_edge_properties_key_value_edge
                    ON graph_edge_properties(key, value, edge_key);

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

                PRAGMA user_version = {SQLITE_GRAPH_SCHEMA_VERSION};
                "#,
            )).await.context("initializing libsql graph schema")?;
            Ok::<(), anyhow::Error>(())
        })?;
        Ok(())
    }
}

fn to_json<T: serde::Serialize>(value: &T) -> Result<String> {
    serde_json::to_string(value).map_err(Into::into)
}

fn optional_to_json<T: serde::Serialize>(value: &Option<T>) -> Result<Option<String>> {
    value.as_ref().map(to_json).transpose()
}

fn node_from_row(row: &libsql::Row) -> Result<GraphNode> {
    let id: String = row.get(0)?;
    let kind: String = row.get(1)?;
    let label: String = row.get(2)?;
    let properties_json: String = row.get(3)?;
    let provenance_json: String = row.get(4)?;
    let freshness_json: Option<String> = row.get(5)?;
    Ok(GraphNode {
        id,
        kind,
        label,
        properties: serde_json::from_str(&properties_json)?,
        provenance: serde_json::from_str(&provenance_json)?,
        freshness: freshness_json
            .map(|v| serde_json::from_str(&v))
            .transpose()?,
    })
}

fn edge_from_row(row: &libsql::Row) -> Result<GraphEdge> {
    let edge_key: String = row.get(0)?;
    let from_id: String = row.get(1)?;
    let to_id: String = row.get(2)?;
    let kind: String = row.get(3)?;
    let properties_json: String = row.get(4)?;
    let provenance_json: String = row.get(5)?;
    let freshness_json: Option<String> = row.get(6)?;
    Ok(GraphEdge {
        id: edge_key,
        from_id,
        to_id,
        kind,
        properties: serde_json::from_str(&properties_json)?,
        provenance: serde_json::from_str(&provenance_json)?,
        freshness: freshness_json
            .map(|v| serde_json::from_str(&v))
            .transpose()?,
    })
}

fn stable_graph_edge_id(from_id: &str, to_id: &str, kind: &str) -> String {
    let raw = serde_json::json!([from_id, kind, to_id]).to_string();
    format!("edge:{}", blake3::hash(raw.as_bytes()).to_hex())
}

fn row_hash<T: serde::Serialize>(value: &T) -> Result<String> {
    let payload = serde_json::to_vec(value)?;
    Ok(blake3::hash(&payload).to_hex().to_string())
}

fn replace_node_properties(conn: &libsql::Connection, rt: &tokio::runtime::Runtime, node_id: &str, properties: &BTreeMap<String, String>) -> Result<()> {
    block_on(rt, async {
        conn.execute("DELETE FROM graph_node_properties WHERE node_id = ?1", [node_id]).await?;
        let stmt = conn.prepare(
            "INSERT INTO graph_node_properties (node_id, key, value) VALUES (?1, ?2, ?3)"
        ).await?;
        for (key, value) in properties {
            stmt.execute(libsql::params![node_id.to_string(), key.clone(), value.clone()]).await?;
        }
        Ok::<(), anyhow::Error>(())
    })?;
    Ok(())
}

fn replace_edge_properties(conn: &libsql::Connection, rt: &tokio::runtime::Runtime, edge_key: &str, properties: &BTreeMap<String, String>) -> Result<()> {
    block_on(rt, async {
        conn.execute("DELETE FROM graph_edge_properties WHERE edge_key = ?1", [edge_key.to_string()]).await?;
        let stmt = conn.prepare(
            "INSERT INTO graph_edge_properties (edge_key, key, value) VALUES (?1, ?2, ?3)"
        ).await?;
        for (key, value) in properties {
            stmt.execute(libsql::params![edge_key.to_string(), key.clone(), value.clone()]).await?;
        }
        Ok::<(), anyhow::Error>(())
    })?;
    Ok(())
}

impl GraphStore for LibsqlGraphStore {
    fn upsert_node(&self, node: &GraphNode) -> Result<()> {
        let id = node.id.clone();
        block_on(&self.rt, async {
            let properties_json = to_json(&node.properties)?;
            let provenance_json = to_json(&node.provenance)?;
            let freshness_json = optional_to_json(&node.freshness)?;
            let hash = row_hash(node)?;
            self.conn.execute(
                r#"
                INSERT INTO graph_nodes
                    (id, kind, label, properties_json, provenance_json, freshness_json, row_hash, source_watermark)
                VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, NULL)
                ON CONFLICT(id) DO UPDATE SET
                    kind = excluded.kind,
                    label = excluded.label,
                    properties_json = excluded.properties_json,
                    provenance_json = excluded.provenance_json,
                    freshness_json = excluded.freshness_json,
                    row_hash = excluded.row_hash,
                    source_watermark = excluded.source_watermark
                "#,
                libsql::params![node.id.clone(), node.kind.clone(), node.label.clone(), properties_json, provenance_json, freshness_json, hash],
            ).await?;
            Ok::<(), anyhow::Error>(())
        })?;
        replace_node_properties(&self.conn, &self.rt, &id, &node.properties)?;
        Ok(())
    }

    fn upsert_edge(&self, edge: &GraphEdge) -> Result<()> {
        let edge_key = if edge.id.is_empty() {
            stable_graph_edge_id(&edge.from_id, &edge.to_id, &edge.kind)
        } else {
            edge.id.clone()
        };
        let edge_key_for_props = edge_key.clone();
        block_on(&self.rt, async {
            let properties_json = to_json(&edge.properties)?;
            let provenance_json = to_json(&edge.provenance)?;
            let freshness_json = optional_to_json(&edge.freshness)?;
            let hash = row_hash(edge)?;
            self.conn.execute(
                r#"
                INSERT INTO graph_edges
                    (edge_key, from_id, to_id, kind, properties_json, provenance_json, freshness_json, row_hash, source_watermark)
                VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, NULL)
                ON CONFLICT(from_id, to_id, kind) DO UPDATE SET
                    edge_key = excluded.edge_key,
                    properties_json = excluded.properties_json,
                    provenance_json = excluded.provenance_json,
                    freshness_json = excluded.freshness_json,
                    row_hash = excluded.row_hash,
                    source_watermark = excluded.source_watermark
                "#,
                libsql::params![edge_key, edge.from_id.clone(), edge.to_id.clone(), edge.kind.clone(), properties_json, provenance_json, freshness_json, hash],
            ).await?;
            Ok::<(), anyhow::Error>(())
        })?;
        replace_edge_properties(&self.conn, &self.rt, &edge_key_for_props, &edge.properties)?;
        Ok(())
    }

    fn delete_node(&self, id: &str) -> Result<usize> {
        let count = block_on(&self.rt, async {
            let result = self.conn.execute(
                "DELETE FROM graph_nodes WHERE id = ?1",
                [id],
            ).await?;
            Ok::<u64, anyhow::Error>(result)
        })?;
        Ok(count as usize)
    }

    fn delete_edge(&self, from_id: &str, to_id: &str, kind: &str) -> Result<usize> {
        let count = block_on(&self.rt, async {
            let result = self.conn.execute(
                "DELETE FROM graph_edges WHERE from_id = ?1 AND to_id = ?2 AND kind = ?3",
                libsql::params![from_id, to_id, kind],
            ).await?;
            Ok::<u64, anyhow::Error>(result)
        })?;
        Ok(count as usize)
    }

    fn node(&self, id: &str) -> Result<Option<GraphNode>> {
        block_on(&self.rt, async {
            let mut rows = self.conn.query(
                r#"
                SELECT id, kind, label, properties_json, provenance_json, freshness_json
                FROM graph_nodes
                WHERE id = ?1
                "#,
                [id],
            ).await?;
            match rows.next().await? {
                Some(row) => Ok(Some(node_from_row(&row)?)),
                None => Ok(None),
            }
        })
    }

    fn all_nodes(&self) -> Result<Vec<GraphNode>> {
        block_on(&self.rt, async {
            let mut rows = self.conn.query(
                r#"
                SELECT id, kind, label, properties_json, provenance_json, freshness_json
                FROM graph_nodes
                ORDER BY id
                "#,
                (),
            ).await?;
            let mut nodes = Vec::new();
            while let Some(row) = rows.next().await? {
                nodes.push(node_from_row(&row)?);
            }
            Ok(nodes)
        })
    }

    fn all_edges(&self) -> Result<Vec<GraphEdge>> {
        block_on(&self.rt, async {
            let mut rows = self.conn.query(
                r#"
                SELECT edge_key, from_id, to_id, kind, properties_json, provenance_json, freshness_json
                FROM graph_edges
                ORDER BY from_id, kind, to_id
                "#,
                (),
            ).await?;
            let mut edges = Vec::new();
            while let Some(row) = rows.next().await? {
                edges.push(edge_from_row(&row)?);
            }
            Ok(edges)
        })
    }

    fn nodes_by_kind(&self, kind: &str) -> Result<Vec<GraphNode>> {
        block_on(&self.rt, async {
            let mut rows = self.conn.query(
                r#"
                SELECT id, kind, label, properties_json, provenance_json, freshness_json
                FROM graph_nodes
                WHERE kind = ?1
                ORDER BY id
                "#,
                [kind],
            ).await?;
            let mut nodes = Vec::new();
            while let Some(row) = rows.next().await? {
                nodes.push(node_from_row(&row)?);
            }
            Ok(nodes)
        })
    }

    fn outgoing_edges(&self, from_id: &str, kind: Option<&str>) -> Result<Vec<GraphEdge>> {
        block_on(&self.rt, async {
            let mut edges = Vec::new();
            match kind {
                Some(kind) => {
                    let mut rows = self.conn.query(
                        r#"
                        SELECT edge_key, from_id, to_id, kind, properties_json, provenance_json, freshness_json
                        FROM graph_edges
                        WHERE from_id = ?1 AND kind = ?2
                        ORDER BY to_id, kind
                        "#,
                        libsql::params![from_id, kind],
                    ).await?;
                    while let Some(row) = rows.next().await? {
                        edges.push(edge_from_row(&row)?);
                    }
                }
                None => {
                    let mut rows = self.conn.query(
                        r#"
                        SELECT edge_key, from_id, to_id, kind, properties_json, provenance_json, freshness_json
                        FROM graph_edges
                        WHERE from_id = ?1
                        ORDER BY to_id, kind
                        "#,
                        [from_id],
                    ).await?;
                    while let Some(row) = rows.next().await? {
                        edges.push(edge_from_row(&row)?);
                    }
                }
            }
            Ok(edges)
        })
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
        if from_id == to_id {
            return Ok(Some(GraphPath {
                nodes: vec![from_id.to_string()],
                hops: 0,
            }));
        }
        let hop_limit = max_hops.unwrap_or(usize::MAX);
        if hop_limit == 0 {
            return Ok(None);
        }

        let mut visited = BTreeSet::from([from_id.to_string()]);
        let mut parent = BTreeMap::<String, String>::from([(from_id.to_string(), String::new())]);
        let mut frontier = vec![from_id.to_string()];

        for _depth in 0..hop_limit {
            if frontier.is_empty() {
                break;
            }
            let mut next_frontier = BTreeSet::new();
            for current in &frontier {
                let neighbors = self.outgoing_edges(current, kind)?;
                for edge in neighbors {
                    if !visited.insert(edge.to_id.clone()) {
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
                        return Ok(Some(GraphPath {
                            hops: nodes.len().saturating_sub(1),
                            nodes,
                        }));
                    }
                    next_frontier.insert(edge.to_id);
                }
            }
            frontier = next_frontier.into_iter().collect();
        }
        Ok(None)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tsift_core::{GraphFreshness, GraphProjection, GraphProvenance};

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
                GraphEdge::new("topic:rooms", "topic:egress", "related_to")
                    .with_provenance(source),
            ],
        }
    }

    #[test]
    fn libsql_store_round_trips_generic_nodes_edges() {
        let store = LibsqlGraphStore::in_memory().unwrap();
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
        assert_eq!(
            store.nodes_by_kind("topic").unwrap(),
            vec![topic]
        );
        assert_eq!(store.all_nodes().unwrap().len(), 2);
        assert_eq!(store.all_edges().unwrap().len(), 1);
        assert_eq!(
            store.outgoing_edges("doc:livekit", Some("mentions")).unwrap(),
            vec![edge]
        );
    }

    #[test]
    fn libsql_store_supports_projection_upsert() {
        let store = LibsqlGraphStore::in_memory().unwrap();
        let projection = sample_projection();
        projection.upsert_into(&store).unwrap();

        assert_eq!(store.node("doc:livekit").unwrap().unwrap().kind, "document");
        assert_eq!(store.nodes_by_kind("topic").unwrap().len(), 2);
        let mentions = store
            .outgoing_edges("doc:livekit", Some("mentions"))
            .unwrap();
        assert_eq!(mentions.len(), 1);
        assert_eq!(mentions[0].to_id, "topic:rooms");
    }

    #[test]
    fn libsql_store_crud_neighborhood_and_ordering() {
        let store = LibsqlGraphStore::in_memory().unwrap();
        let projection = sample_projection();
        projection.upsert_into(&store).unwrap();

        let neighborhood = store.neighborhood("doc:livekit", 2, None).unwrap().unwrap();
        let node_ids: Vec<&str> = neighborhood
            .nodes
            .iter()
            .map(|n| n.id.as_str())
            .collect();
        assert_eq!(node_ids, vec!["doc:livekit", "topic:egress", "topic:rooms"]);

        assert_eq!(
            store.delete_edge("topic:rooms", "topic:egress", "related_to").unwrap(),
            1
        );
        assert!(store
            .shortest_path("doc:livekit", "topic:egress", None)
            .unwrap()
            .is_none());
        assert_eq!(store.delete_node("topic:rooms").unwrap(), 1);
        assert!(store.node("topic:rooms").unwrap().is_none());
        assert!(store.outgoing_edges("doc:livekit", None).unwrap().is_empty());
    }

    #[test]
    fn libsql_store_shortest_path() {
        let store = LibsqlGraphStore::in_memory().unwrap();
        for id in ["a", "b", "c"] {
            store.upsert_node(&GraphNode::new(id, "symbol", id)).unwrap();
        }
        store.upsert_edge(&GraphEdge::new("a", "b", "calls")).unwrap();
        store.upsert_edge(&GraphEdge::new("a", "c", "documents")).unwrap();
        store.upsert_edge(&GraphEdge::new("b", "c", "calls")).unwrap();

        let calls = store.outgoing_edges("a", Some("calls")).unwrap();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].to_id, "b");

        let path = store
            .shortest_path("a", "c", Some("calls"))
            .unwrap()
            .unwrap();
        assert_eq!(path.nodes, vec!["a", "b", "c"]);
        assert_eq!(path.hops, 2);

        assert!(store.shortest_path("c", "a", Some("calls")).unwrap().is_none());
    }

    #[test]
    fn libsql_store_open_creates_db_file() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("test-graph.db");
        let store = LibsqlGraphStore::open(&db_path).unwrap();
        store
            .upsert_node(&GraphNode::new("test", "test", "test"))
            .unwrap();
        assert!(db_path.exists());
    }

    #[test]
    fn libsql_graph_counts() {
        let store = LibsqlGraphStore::in_memory().unwrap();
        for id in ["a", "b", "c"] {
            store.upsert_node(&GraphNode::new(id, "symbol", id)).unwrap();
        }
        store.upsert_edge(&GraphEdge::new("a", "b", "calls")).unwrap();
        store.upsert_edge(&GraphEdge::new("b", "c", "calls")).unwrap();
        let (nodes, edges) = store.graph_counts().unwrap();
        assert_eq!(nodes, 3);
        assert_eq!(edges, 2);
    }
}
