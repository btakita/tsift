use anyhow::{Context, Result};
use rusqlite::{Connection, OpenFlags, OptionalExtension, Row};
use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::path::{Path, PathBuf};
use std::time::Duration;
use tsift_core::{
    GraphEdge, GraphNode, GraphPath, GraphPagedSubgraph, GraphPropertyFilter, GraphQueryOptions,
    GraphStore, GraphSubgraph,
};

pub struct TokensaveDb {
    conn: Connection,
    db_path: PathBuf,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct TokensaveNode {
    pub id: String,
    pub kind: String,
    pub name: String,
    pub qualified_name: String,
    pub file_path: String,
    pub start_line: i64,
    pub end_line: i64,
    pub start_column: i64,
    pub end_column: i64,
    pub docstring: Option<String>,
    pub signature: Option<String>,
    pub visibility: String,
    pub is_async: bool,
    pub branches: i64,
    pub loops: i64,
    pub returns: i64,
    pub max_nesting: i64,
    pub unsafe_blocks: i64,
    pub parent_id: Option<String>,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct TokensaveEdge {
    pub id: i64,
    pub source: String,
    pub target: String,
    pub kind: String,
    pub line: Option<i64>,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct TokensaveFile {
    pub path: String,
    pub content_hash: String,
    pub size: i64,
    pub node_count: i64,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct FtsSearchResult {
    pub node_id: String,
    pub name: String,
    pub qualified_name: String,
    pub docstring: Option<String>,
    pub signature: Option<String>,
    pub rank: f64,
}

impl TokensaveDb {
    pub fn open(db_path: &Path) -> Result<Self> {
        let conn = Connection::open_with_flags(
            db_path,
            OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )
        .with_context(|| format!("opening tokensave db: {}", db_path.display()))?;
        conn.busy_timeout(Duration::from_secs(5))?;
        Ok(Self {
            conn,
            db_path: db_path.to_path_buf(),
        })
    }

    pub fn discover(project_dir: &Path) -> Result<Option<Self>> {
        let db_path = project_dir.join(".tokensave").join("tokensave.db");
        if !db_path.exists() {
            return Ok(None);
        }
        Ok(Some(Self::open(&db_path)?))
    }

    pub fn db_path(&self) -> &Path {
        &self.db_path
    }

    pub fn conn(&self) -> &Connection {
        &self.conn
    }

    pub fn node_count(&self) -> Result<usize> {
        Ok(self
            .conn
            .query_row("SELECT COUNT(*) FROM nodes", [], |row| row.get::<_, usize>(0))?)
    }

    pub fn edge_count(&self) -> Result<usize> {
        Ok(self
            .conn
            .query_row("SELECT COUNT(*) FROM edges", [], |row| row.get::<_, usize>(0))?)
    }

    pub fn file_count(&self) -> Result<usize> {
        Ok(self
            .conn
            .query_row("SELECT COUNT(*) FROM files", [], |row| row.get::<_, usize>(0))?)
    }

    pub fn kinds(&self) -> Result<Vec<(String, usize)>> {
        let mut stmt = self
            .conn
            .prepare("SELECT kind, COUNT(*) FROM nodes GROUP BY kind ORDER BY COUNT(*) DESC")?;
        let rows = stmt.query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, usize>(1)?))
        })?;
        rows.collect::<Result<Vec<_>, _>>().map_err(Into::into)
    }

    pub fn edge_kinds(&self) -> Result<Vec<(String, usize)>> {
        let mut stmt = self
            .conn
            .prepare("SELECT kind, COUNT(*) FROM edges GROUP BY kind ORDER BY COUNT(*) DESC")?;
        let rows = stmt.query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, usize>(1)?))
        })?;
        rows.collect::<Result<Vec<_>, _>>().map_err(Into::into)
    }

    pub fn nodes_by_kind(&self, kind: &str) -> Result<Vec<TokensaveNode>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, kind, name, qualified_name, file_path, start_line, end_line, \
             start_column, end_column, docstring, signature, visibility, is_async, \
             branches, loops, returns, max_nesting, unsafe_blocks, parent_id \
             FROM nodes WHERE kind = ?1 ORDER BY qualified_name",
        )?;
        let rows = stmt.query_map([kind], tokensave_node_from_row)?;
        rows.collect::<Result<Vec<_>, _>>().map_err(Into::into)
    }

    pub fn node_by_id(&self, id: &str) -> Result<Option<TokensaveNode>> {
        self.conn
            .query_row(
                "SELECT id, kind, name, qualified_name, file_path, start_line, end_line, \
                 start_column, end_column, docstring, signature, visibility, is_async, \
                 branches, loops, returns, max_nesting, unsafe_blocks, parent_id \
                 FROM nodes WHERE id = ?1",
                [id],
                tokensave_node_from_row,
            )
            .optional()
            .map_err(Into::into)
    }

    pub fn edges_from(&self, source_id: &str, kind: Option<&str>) -> Result<Vec<TokensaveEdge>> {
        let sql = match kind {
            Some(_) => "SELECT id, source, target, kind, line FROM edges WHERE source = ?1 AND kind = ?2 ORDER BY target",
            None => "SELECT id, source, target, kind, line FROM edges WHERE source = ?1 ORDER BY kind, target",
        };
        let mut stmt = self.conn.prepare(sql)?;
        let rows = match kind {
            Some(k) => stmt.query_map((source_id, k), tokensave_edge_from_row)?,
            None => stmt.query_map([source_id], tokensave_edge_from_row)?,
        };
        rows.collect::<Result<Vec<_>, _>>().map_err(Into::into)
    }

    pub fn edges_to(&self, target_id: &str, kind: Option<&str>) -> Result<Vec<TokensaveEdge>> {
        let sql = match kind {
            Some(_) => "SELECT id, source, target, kind, line FROM edges WHERE target = ?1 AND kind = ?2 ORDER BY source",
            None => "SELECT id, source, target, kind, line FROM edges WHERE target = ?1 ORDER BY kind, source",
        };
        let mut stmt = self.conn.prepare(sql)?;
        let rows = match kind {
            Some(k) => stmt.query_map((target_id, k), tokensave_edge_from_row)?,
            None => stmt.query_map([target_id], tokensave_edge_from_row)?,
        };
        rows.collect::<Result<Vec<_>, _>>().map_err(Into::into)
    }

    pub fn files(&self) -> Result<Vec<TokensaveFile>> {
        let mut stmt = self.conn.prepare(
            "SELECT path, content_hash, size, node_count FROM files ORDER BY path",
        )?;
        let rows = stmt.query_map([], |row| {
            Ok(TokensaveFile {
                path: row.get(0)?,
                content_hash: row.get(1)?,
                size: row.get(2)?,
                node_count: row.get(3)?,
            })
        })?;
        rows.collect::<Result<Vec<_>, _>>().map_err(Into::into)
    }

    pub fn nodes_for_file(&self, file_path: &str) -> Result<Vec<TokensaveNode>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, kind, name, qualified_name, file_path, start_line, end_line, \
             start_column, end_column, docstring, signature, visibility, is_async, \
             branches, loops, returns, max_nesting, unsafe_blocks, parent_id \
             FROM nodes WHERE file_path = ?1 ORDER BY start_line",
        )?;
        let rows = stmt.query_map([file_path], tokensave_node_from_row)?;
        rows.collect::<Result<Vec<_>, _>>().map_err(Into::into)
    }

    pub fn search_fts(&self, query: &str, limit: Option<usize>) -> Result<Vec<FtsSearchResult>> {
        let limit_clause = match limit {
            Some(n) => format!("LIMIT {n}"),
            None => String::new(),
        };
        let sql = format!(
            "SELECT n.id, n.name, n.qualified_name, n.docstring, n.signature, rank \
             FROM nodes_fts f \
             JOIN nodes n ON n.rowid = f.rowid \
             WHERE nodes_fts MATCH ?1 \
             ORDER BY rank \
             {limit_clause}"
        );
        let mut stmt = self.conn.prepare(&sql)?;
        let rows = stmt.query_map([query], |row| {
            Ok(FtsSearchResult {
                node_id: row.get(0)?,
                name: row.get(1)?,
                qualified_name: row.get(2)?,
                docstring: row.get(3)?,
                signature: row.get(4)?,
                rank: row.get(5)?,
            })
        })?;
        rows.collect::<Result<Vec<_>, _>>().map_err(Into::into)
    }

    pub fn to_graph_node(tokensave_node: &TokensaveNode) -> GraphNode {
        let mut properties = BTreeMap::new();
        properties.insert("qualified_name".to_string(), tokensave_node.qualified_name.clone());
        properties.insert("file_path".to_string(), tokensave_node.file_path.clone());
        properties.insert("start_line".to_string(), tokensave_node.start_line.to_string());
        properties.insert("end_line".to_string(), tokensave_node.end_line.to_string());
        properties.insert("visibility".to_string(), tokensave_node.visibility.clone());
        if tokensave_node.is_async {
            properties.insert("is_async".to_string(), "true".to_string());
        }
        if tokensave_node.branches > 0 {
            properties.insert("branches".to_string(), tokensave_node.branches.to_string());
        }
        if tokensave_node.loops > 0 {
            properties.insert("loops".to_string(), tokensave_node.loops.to_string());
        }
        if let Some(doc) = &tokensave_node.docstring {
            properties.insert("docstring".to_string(), doc.clone());
        }
        if let Some(sig) = &tokensave_node.signature {
            properties.insert("signature".to_string(), sig.clone());
        }
        GraphNode {
            id: tokensave_node.id.clone(),
            kind: tokensave_node.kind.clone(),
            label: tokensave_node.name.clone(),
            properties,
            provenance: Vec::new(),
            freshness: None,
        }
    }

    pub fn to_graph_edge(tokensave_edge: &TokensaveEdge) -> GraphEdge {
        let mut properties = BTreeMap::new();
        if let Some(line) = tokensave_edge.line {
            properties.insert("line".to_string(), line.to_string());
        }
        GraphEdge {
            id: String::new(),
            from_id: tokensave_edge.source.clone(),
            to_id: tokensave_edge.target.clone(),
            kind: tokensave_edge.kind.clone(),
            properties,
            provenance: Vec::new(),
            freshness: None,
        }
    }
}

fn tokensave_node_from_row(row: &Row<'_>) -> rusqlite::Result<TokensaveNode> {
    Ok(TokensaveNode {
        id: row.get(0)?,
        kind: row.get(1)?,
        name: row.get(2)?,
        qualified_name: row.get(3)?,
        file_path: row.get(4)?,
        start_line: row.get(5)?,
        end_line: row.get(6)?,
        start_column: row.get(7)?,
        end_column: row.get(8)?,
        docstring: row.get(9)?,
        signature: row.get(10)?,
        visibility: row.get(11)?,
        is_async: row.get::<_, i64>(12)? != 0,
        branches: row.get(13)?,
        loops: row.get(14)?,
        returns: row.get(15)?,
        max_nesting: row.get(16)?,
        unsafe_blocks: row.get(17)?,
        parent_id: row.get(18)?,
    })
}

fn tokensave_edge_from_row(row: &Row<'_>) -> rusqlite::Result<TokensaveEdge> {
    Ok(TokensaveEdge {
        id: row.get(0)?,
        source: row.get(1)?,
        target: row.get(2)?,
        kind: row.get(3)?,
        line: row.get(4)?,
    })
}

impl GraphStore for TokensaveDb {
    fn upsert_node(&self, _node: &GraphNode) -> Result<()> {
        anyhow::bail!("tokensave adapter is read-only")
    }

    fn upsert_edge(&self, _edge: &GraphEdge) -> Result<()> {
        anyhow::bail!("tokensave adapter is read-only")
    }

    fn delete_node(&self, _id: &str) -> Result<usize> {
        anyhow::bail!("tokensave adapter is read-only")
    }

    fn delete_edge(&self, _from_id: &str, _to_id: &str, _kind: &str) -> Result<usize> {
        anyhow::bail!("tokensave adapter is read-only")
    }

    fn node(&self, id: &str) -> Result<Option<GraphNode>> {
        match self.node_by_id(id)? {
            Some(ts_node) => Ok(Some(Self::to_graph_node(&ts_node))),
            None => Ok(None),
        }
    }

    fn all_nodes(&self) -> Result<Vec<GraphNode>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, kind, name, qualified_name, file_path, start_line, end_line, \
             start_column, end_column, docstring, signature, visibility, is_async, \
             branches, loops, returns, max_nesting, unsafe_blocks, parent_id \
             FROM nodes ORDER BY id",
        )?;
        let rows = stmt.query_map([], tokensave_node_from_row)?;
        let mut nodes: Vec<GraphNode> = rows
            .map(|row| Ok(Self::to_graph_node(&row?)))
            .collect::<Result<Vec<_>>>()?;
        nodes.sort_by(|a, b| a.id.cmp(&b.id));
        Ok(nodes)
    }

    fn all_edges(&self) -> Result<Vec<GraphEdge>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, source, target, kind, line FROM edges ORDER BY source, kind, target",
        )?;
        let rows = stmt.query_map([], tokensave_edge_from_row)?;
        let mut edges: Vec<GraphEdge> = rows
            .map(|row| Ok(Self::to_graph_edge(&row?)))
            .collect::<Result<Vec<_>>>()?;
        edges.sort_by(|a, b| {
            a.from_id
                .cmp(&b.from_id)
                .then(a.kind.cmp(&b.kind))
                .then(a.to_id.cmp(&b.to_id))
        });
        Ok(edges)
    }

    fn graph_counts(&self) -> Result<(usize, usize)> {
        Ok((self.node_count()?, self.edge_count()?))
    }

    fn nodes_by_kind(&self, kind: &str) -> Result<Vec<GraphNode>> {
        let ts_nodes = self.nodes_by_kind(kind)?;
        let mut nodes: Vec<GraphNode> = ts_nodes
            .iter()
            .map(Self::to_graph_node)
            .collect();
        nodes.sort_by(|a, b| a.id.cmp(&b.id));
        Ok(nodes)
    }

    fn outgoing_edges(&self, from_id: &str, kind: Option<&str>) -> Result<Vec<GraphEdge>> {
        let ts_edges = self.edges_from(from_id, kind)?;
        let mut edges: Vec<GraphEdge> = ts_edges.iter().map(Self::to_graph_edge).collect();
        edges.sort_by(|a, b| {
            a.to_id
                .cmp(&b.to_id)
                .then(a.kind.cmp(&b.kind))
        });
        Ok(edges)
    }

    fn shortest_path(
        &self,
        from_id: &str,
        to_id: &str,
        kind: Option<&str>,
    ) -> Result<Option<GraphPath>> {
        if from_id == to_id {
            return Ok(Some(GraphPath {
                nodes: vec![from_id.to_string()],
                hops: 0,
            }));
        }
        let mut visited = BTreeSet::from([from_id.to_string()]);
        let mut parent = BTreeMap::<String, String>::from([(from_id.to_string(), String::new())]);
        let mut frontier = vec![from_id.to_string()];

        while !frontier.is_empty() {
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
    use std::fs;

    fn setup_tokensave_db(dir: &std::path::Path) -> PathBuf {
        let tokensave_dir = dir.join(".tokensave");
        fs::create_dir_all(&tokensave_dir).unwrap();
        let db_path = tokensave_dir.join("tokensave.db");
        let conn = Connection::open(&db_path).unwrap();
        conn.execute_batch(
            r#"
            CREATE TABLE nodes (
                id TEXT PRIMARY KEY,
                kind TEXT NOT NULL,
                name TEXT NOT NULL,
                qualified_name TEXT NOT NULL,
                file_path TEXT NOT NULL,
                start_line INTEGER NOT NULL,
                end_line INTEGER NOT NULL,
                start_column INTEGER NOT NULL DEFAULT 0,
                end_column INTEGER NOT NULL DEFAULT 0,
                docstring TEXT,
                signature TEXT,
                visibility TEXT NOT NULL DEFAULT 'private',
                is_async INTEGER NOT NULL DEFAULT 0,
                branches INTEGER NOT NULL DEFAULT 0,
                loops INTEGER NOT NULL DEFAULT 0,
                returns INTEGER NOT NULL DEFAULT 0,
                max_nesting INTEGER NOT NULL DEFAULT 0,
                unsafe_blocks INTEGER NOT NULL DEFAULT 0,
                unchecked_calls INTEGER NOT NULL DEFAULT 0,
                assertions INTEGER NOT NULL DEFAULT 0,
                updated_at INTEGER NOT NULL DEFAULT 0,
                attrs_start_line INTEGER NOT NULL DEFAULT 0,
                parent_id TEXT
            );
            CREATE TABLE edges (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                source TEXT NOT NULL,
                target TEXT NOT NULL,
                kind TEXT NOT NULL,
                line INTEGER,
                FOREIGN KEY (source) REFERENCES nodes(id) ON DELETE CASCADE,
                FOREIGN KEY (target) REFERENCES nodes(id) ON DELETE CASCADE
            );
            CREATE TABLE files (
                path TEXT PRIMARY KEY,
                content_hash TEXT NOT NULL,
                size INTEGER NOT NULL,
                modified_at INTEGER NOT NULL,
                indexed_at INTEGER NOT NULL,
                node_count INTEGER NOT NULL DEFAULT 0
            );
            CREATE VIRTUAL TABLE nodes_fts USING fts5(
                name, qualified_name, docstring, signature,
                content='nodes', content_rowid='rowid'
            );
            CREATE INDEX idx_nodes_kind ON nodes(kind);
            CREATE INDEX idx_nodes_name ON nodes(name);
            CREATE INDEX idx_edges_source_kind ON edges(source, kind);
            CREATE INDEX idx_edges_target_kind ON edges(target, kind);
            "#,
        )
        .unwrap();

        conn.execute(
            "INSERT INTO nodes (id, kind, name, qualified_name, file_path, start_line, end_line) \
             VALUES ('fn:main', 'function', 'main', 'main', 'src/main.rs', 1, 10)",
            [],
        ).unwrap();
        conn.execute(
            "INSERT INTO nodes (id, kind, name, qualified_name, file_path, start_line, end_line, docstring, is_async) \
             VALUES ('fn:alpha', 'function', 'alpha', 'alpha', 'src/lib.rs', 5, 15, 'Does the thing', 1)",
            [],
        ).unwrap();
        conn.execute(
            "INSERT INTO nodes (id, kind, name, qualified_name, file_path, start_line, end_line) \
             VALUES ('fn:beta', 'function', 'beta', 'beta', 'src/lib.rs', 20, 30)",
            [],
        ).unwrap();
        conn.execute(
            "INSERT INTO nodes (id, kind, name, qualified_name, file_path, start_line, end_line) \
             VALUES ('struct:Config', 'struct', 'Config', 'Config', 'src/lib.rs', 35, 40)",
            [],
        ).unwrap();
        conn.execute(
            "INSERT INTO edges (source, target, kind, line) VALUES ('fn:main', 'fn:alpha', 'calls', 3)",
            [],
        ).unwrap();
        conn.execute(
            "INSERT INTO edges (source, target, kind, line) VALUES ('fn:alpha', 'fn:beta', 'calls', 8)",
            [],
        ).unwrap();
        conn.execute(
            "INSERT INTO edges (source, target, kind) VALUES ('fn:alpha', 'struct:Config', 'type_of')",
            [],
        ).unwrap();
        conn.execute(
            "INSERT INTO nodes_fts (rowid, name, qualified_name, docstring, signature) \
             VALUES (1, 'main', 'main', NULL, NULL)",
            [],
        ).unwrap();
        conn.execute(
            "INSERT INTO nodes_fts (rowid, name, qualified_name, docstring, signature) \
             VALUES (2, 'alpha', 'alpha', 'Does the thing', 'fn alpha() -> Result<()>')",
            [],
        ).unwrap();
        conn.execute(
            "INSERT INTO nodes_fts (rowid, name, qualified_name, docstring, signature) \
             VALUES (3, 'beta', 'beta', NULL, NULL)",
            [],
        ).unwrap();
        conn.execute(
            "INSERT INTO files (path, content_hash, size, modified_at, indexed_at, node_count) \
             VALUES ('src/lib.rs', 'abc123', 500, 1000, 1001, 3)",
            [],
        ).unwrap();

        drop(conn);
        db_path
    }

    #[test]
    fn tokensave_db_reads_nodes_and_edges() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = setup_tokensave_db(dir.path());
        let db = TokensaveDb::open(&db_path).unwrap();

        assert_eq!(db.node_count().unwrap(), 4);
        assert_eq!(db.edge_count().unwrap(), 3);
        assert_eq!(db.file_count().unwrap(), 1);

        let functions = db.nodes_by_kind("function").unwrap();
        assert_eq!(functions.len(), 3);
        let names: Vec<&str> = functions.iter().map(|n| n.name.as_str()).collect();
        assert!(names.contains(&"main"));
        assert!(names.contains(&"alpha"));
        assert!(names.contains(&"beta"));

        let calls = db.edges_from("fn:alpha", Some("calls")).unwrap();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].target, "fn:beta");
    }

    #[test]
    fn tokensave_db_fts_search() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = setup_tokensave_db(dir.path());
        let db = TokensaveDb::open(&db_path).unwrap();

        let results = db.search_fts("alpha", Some(10)).unwrap();
        assert!(!results.is_empty());
        assert_eq!(results[0].name, "alpha");
        assert_eq!(
            results[0].docstring.as_deref(),
            Some("Does the thing")
        );
    }

    #[test]
    fn tokensave_graph_store_adapter() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = setup_tokensave_db(dir.path());
        let db = TokensaveDb::open(&db_path).unwrap();

        let (nodes, edges) = db.graph_counts().unwrap();
        assert_eq!(nodes, 4);
        assert_eq!(edges, 3);

        let node = db.node("fn:alpha").unwrap().unwrap();
        assert_eq!(node.kind, "function");
        assert_eq!(node.label, "alpha");
        assert_eq!(node.properties.get("is_async"), Some(&"true".to_string()));

        let path = db.shortest_path("fn:main", "fn:beta", Some("calls"))
            .unwrap()
            .unwrap();
        assert_eq!(path.nodes, vec!["fn:main", "fn:alpha", "fn:beta"]);
        assert_eq!(path.hops, 2);
    }

    #[test]
    fn tokensave_discover_finds_db() {
        let dir = tempfile::tempdir().unwrap();
        setup_tokensave_db(dir.path());
        let db = TokensaveDb::discover(dir.path()).unwrap().unwrap();
        assert_eq!(db.node_count().unwrap(), 4);
    }

    #[test]
    fn tokensave_discover_returns_none_when_missing() {
        let dir = tempfile::tempdir().unwrap();
        assert!(TokensaveDb::discover(dir.path()).unwrap().is_none());
    }

    #[test]
    fn tokensave_write_operations_fail() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = setup_tokensave_db(dir.path());
        let db = TokensaveDb::open(&db_path).unwrap();

        let node = GraphNode::new("test", "test", "test");
        assert!(db.upsert_node(&node).is_err());
        assert!(db.delete_node("test").is_err());
    }

    #[test]
    fn tokensave_nodes_for_file() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = setup_tokensave_db(dir.path());
        let db = TokensaveDb::open(&db_path).unwrap();

        let nodes = db.nodes_for_file("src/lib.rs").unwrap();
        assert_eq!(nodes.len(), 3);
    }
}
