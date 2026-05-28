use crate::index::{ReadOnlyRecovery, copy_read_only_snapshot, read_only_snapshot_recovery};
use anyhow::{Context, Result, bail};
use rusqlite::{
    Connection, OpenFlags, OptionalExtension, Row, params_from_iter,
    types::{Type, Value},
};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

pub use tsift_core::{
    GraphEdge, GraphFreshness, GraphNode, GraphPagedSubgraph, GraphPath, GraphProjection,
    GraphPropertyFilter, GraphProvenance, GraphQueryOptions, GraphQueryPage, GraphStore,
    GraphSubgraph, ConvexEdgeRow, ConvexGraphClient, ConvexGraphStore, ConvexNodeRow,
    ConvexProjectionRows, ConvexRowsGraphClient, apply_graph_edge_query_page,
    apply_graph_query_page, graph_edge_id, shortest_path_using_outgoing,
    stable_graph_edge_id,
};

pub const SQLITE_GRAPH_SCHEMA_VERSION: i64 = 5;
const SQLITE_GRAPH_WAL_AUTOCHECKPOINT_PAGES: i64 = 256;
const SQLITE_GRAPH_STAGING_CHUNK_ROWS: usize = 50;

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

fn sqlite_column_exists(conn: &Connection, table: &str, column: &str) -> Result<bool> {
    let mut stmt = conn.prepare(&format!("PRAGMA table_info({table})"))?;
    let rows = stmt.query_map([], |row| row.get::<_, String>(1))?;
    for row in rows {
        if row? == column {
            return Ok(true);
        }
    }
    Ok(false)
}

fn add_column_if_missing(
    conn: &Connection,
    table: &str,
    column: &str,
    definition: &str,
) -> Result<()> {
    if !sqlite_column_exists(conn, table, column)? {
        conn.execute(
            &format!("ALTER TABLE {table} ADD COLUMN {column} {definition}"),
            [],
        )?;
    }
    Ok(())
}

fn backfill_graph_edge_keys(conn: &Connection) -> Result<()> {
    if !sqlite_column_exists(conn, "graph_edges", "edge_key")? {
        return Ok(());
    }
    let rows = {
        let mut stmt = conn.prepare(
            r#"
            SELECT from_id, to_id, kind
            FROM graph_edges
            WHERE edge_key IS NULL OR edge_key = ''
            ORDER BY from_id, kind, to_id
            "#,
        )?;
        collect_rows(stmt.query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
            ))
        })?)?
    };
    let mut update = conn.prepare(
        r#"
        UPDATE graph_edges
        SET edge_key = ?4
        WHERE from_id = ?1 AND to_id = ?2 AND kind = ?3
        "#,
    )?;
    for (from_id, to_id, kind) in rows {
        update.execute((
            &from_id,
            &to_id,
            &kind,
            stable_graph_edge_id(&from_id, &to_id, &kind),
        ))?;
    }
    Ok(())
}

fn migrate_sqlite_graph_schema(conn: &Connection, old_version: i64) -> Result<()> {
    if old_version < 2 {
        add_column_if_missing(conn, "graph_nodes", "row_hash", "TEXT")?;
        add_column_if_missing(conn, "graph_nodes", "source_watermark", "TEXT")?;
        add_column_if_missing(conn, "graph_edges", "row_hash", "TEXT")?;
        add_column_if_missing(conn, "graph_edges", "source_watermark", "TEXT")?;
    }
    if old_version < 3 {
        rebuild_graph_node_properties(conn)?;
    }
    if old_version < 4 {
        ensure_sqlite_graph_operator_stats_schema(conn)?;
    }
    if old_version < 5 {
        add_column_if_missing(conn, "graph_edges", "edge_key", "TEXT")?;
        backfill_graph_edge_keys(conn)?;
        ensure_sqlite_graph_edge_properties_schema(conn)?;
        rebuild_graph_edge_properties(conn)?;
    }
    Ok(())
}

fn ensure_sqlite_graph_operator_stats_schema(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        r#"
        CREATE INDEX IF NOT EXISTS idx_graph_nodes_kind_label
            ON graph_nodes(kind, label, id);
        CREATE TABLE IF NOT EXISTS graph_operator_stats (
            scope TEXT PRIMARY KEY,
            nodes INTEGER NOT NULL,
            edges INTEGER NOT NULL,
            tombstone_nodes INTEGER NOT NULL,
            tombstone_edges INTEGER NOT NULL,
            file_size_bytes INTEGER,
            freelist_bytes INTEGER,
            observed_at_unix INTEGER NOT NULL
        );
        "#,
    )?;
    Ok(())
}

fn ensure_sqlite_graph_edge_properties_schema(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        r#"
        CREATE UNIQUE INDEX IF NOT EXISTS idx_graph_edges_edge_key
            ON graph_edges(edge_key);
        CREATE TABLE IF NOT EXISTS graph_edge_properties (
            edge_key TEXT NOT NULL,
            key TEXT NOT NULL,
            value TEXT NOT NULL,
            PRIMARY KEY (edge_key, key),
            FOREIGN KEY (edge_key) REFERENCES graph_edges(edge_key) ON DELETE CASCADE
        );
        CREATE INDEX IF NOT EXISTS idx_graph_edge_properties_key_value_edge
            ON graph_edge_properties(key, value, edge_key);
        "#,
    )?;
    Ok(())
}

fn replace_node_properties(
    conn: &Connection,
    node_id: &str,
    properties: &BTreeMap<String, String>,
) -> Result<()> {
    conn.execute(
        "DELETE FROM graph_node_properties WHERE node_id = ?1",
        [node_id],
    )?;
    let mut insert = conn.prepare(
        r#"
        INSERT INTO graph_node_properties (node_id, key, value)
        VALUES (?1, ?2, ?3)
        ON CONFLICT(node_id, key) DO UPDATE SET
            value = excluded.value
        "#,
    )?;
    for (key, value) in properties {
        insert.execute((node_id, key, value))?;
    }
    Ok(())
}

fn replace_edge_properties(
    conn: &Connection,
    edge_key: &str,
    properties: &BTreeMap<String, String>,
) -> Result<()> {
    conn.execute(
        "DELETE FROM graph_edge_properties WHERE edge_key = ?1",
        [edge_key],
    )?;
    let mut insert = conn.prepare(
        r#"
        INSERT INTO graph_edge_properties (edge_key, key, value)
        VALUES (?1, ?2, ?3)
        ON CONFLICT(edge_key, key) DO UPDATE SET
            value = excluded.value
        "#,
    )?;
    for (key, value) in properties {
        insert.execute((edge_key, key, value))?;
    }
    Ok(())
}

fn rebuild_graph_node_properties(conn: &Connection) -> Result<()> {
    if !sqlite_column_exists(conn, "graph_nodes", "properties_json")? {
        return Ok(());
    }
    conn.execute_batch(
        r#"
        DELETE FROM graph_node_properties;
        INSERT INTO graph_node_properties (node_id, key, value)
        SELECT graph_nodes.id, json_each.key, CAST(json_each.value AS TEXT)
        FROM graph_nodes, json_each(graph_nodes.properties_json)
        WHERE json_each.key IS NOT NULL
          AND json_each.value IS NOT NULL
        "#,
    )?;
    Ok(())
}

fn rebuild_graph_edge_properties(conn: &Connection) -> Result<()> {
    if !sqlite_column_exists(conn, "graph_edges", "properties_json")?
        || !sqlite_column_exists(conn, "graph_edges", "edge_key")?
    {
        return Ok(());
    }
    conn.execute_batch(
        r#"
        DELETE FROM graph_edge_properties;
        INSERT INTO graph_edge_properties (edge_key, key, value)
        SELECT graph_edges.edge_key, json_each.key, CAST(json_each.value AS TEXT)
        FROM graph_edges, json_each(graph_edges.properties_json)
        WHERE graph_edges.edge_key IS NOT NULL
          AND graph_edges.edge_key <> ''
          AND json_each.key IS NOT NULL
          AND json_each.value IS NOT NULL
        "#,
    )?;
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
pub struct SqliteProjectionRefreshPhase {
    pub name: String,
    pub duration_micros: u128,
    pub detail: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SqliteProjectionRefresh {
    pub scope: String,
    pub projection_version: String,
    pub source_watermark: Option<String>,
    pub tombstoned_nodes: Vec<String>,
    pub tombstoned_edges: Vec<String>,
    pub upserted_nodes: usize,
    pub upserted_edges: usize,
    pub unchanged_nodes: usize,
    pub unchanged_edges: usize,
    pub upserted_properties: usize,
    pub unchanged_properties: usize,
    pub deleted_properties: usize,
    pub deleted_nodes: usize,
    pub deleted_edges: usize,
    pub pruned_tombstones: usize,
    pub file_size_bytes_before: Option<u64>,
    pub file_size_bytes_after: Option<u64>,
    pub phase_timings: Vec<SqliteProjectionRefreshPhase>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SqliteProjectionVersion {
    pub projection_version: String,
    pub content_hash: Option<String>,
    pub source_watermark: Option<String>,
}

fn sqlite_refresh_phase_timing(
    name: &str,
    started: Instant,
    detail: &str,
) -> SqliteProjectionRefreshPhase {
    SqliteProjectionRefreshPhase {
        name: name.to_string(),
        duration_micros: started.elapsed().as_micros(),
        detail: detail.to_string(),
    }
}

fn sqlite_graph_staging_placeholders(column_count: usize, row_count: usize) -> String {
    let row = format!(
        "({})",
        (0..column_count)
            .map(|_| "?")
            .collect::<Vec<_>>()
            .join(", ")
    );
    (0..row_count)
        .map(|_| row.as_str())
        .collect::<Vec<_>>()
        .join(", ")
}

fn sqlite_stage_projection_nodes(
    tx: &rusqlite::Transaction<'_>,
    nodes: &[GraphNode],
    source_watermark: Option<&str>,
) -> Result<()> {
    for chunk in nodes.chunks(SQLITE_GRAPH_STAGING_CHUNK_ROWS) {
        let sql = format!(
            r#"
            INSERT INTO next_graph_nodes
                (id, kind, label, properties_json, provenance_json, freshness_json, row_hash, source_watermark)
            VALUES {}
            "#,
            sqlite_graph_staging_placeholders(8, chunk.len())
        );
        let mut values = Vec::with_capacity(chunk.len() * 8);
        for node in chunk {
            values.push(Value::Text(node.id.clone()));
            values.push(Value::Text(node.kind.clone()));
            values.push(Value::Text(node.label.clone()));
            values.push(Value::Text(to_json(&node.properties)?));
            values.push(Value::Text(to_json(&node.provenance)?));
            values.push(
                optional_to_json(&node.freshness)?
                    .map(Value::Text)
                    .unwrap_or(Value::Null),
            );
            values.push(Value::Text(row_hash(node)?));
            values.push(
                source_watermark
                    .map(|watermark| Value::Text(watermark.to_string()))
                    .unwrap_or(Value::Null),
            );
        }
        tx.execute(&sql, params_from_iter(values))?;
    }
    Ok(())
}

fn sqlite_stage_projection_edges(
    tx: &rusqlite::Transaction<'_>,
    edges: &[GraphEdge],
    source_watermark: Option<&str>,
) -> Result<()> {
    for chunk in edges.chunks(SQLITE_GRAPH_STAGING_CHUNK_ROWS) {
        let sql = format!(
            r#"
            INSERT INTO next_graph_edges
                (edge_key, from_id, to_id, kind, properties_json, provenance_json, freshness_json, row_hash, source_watermark)
            VALUES {}
            "#,
            sqlite_graph_staging_placeholders(9, chunk.len())
        );
        let mut values = Vec::with_capacity(chunk.len() * 9);
        for edge in chunk {
            values.push(Value::Text(graph_edge_id(edge)));
            values.push(Value::Text(edge.from_id.clone()));
            values.push(Value::Text(edge.to_id.clone()));
            values.push(Value::Text(edge.kind.clone()));
            values.push(Value::Text(to_json(&edge.properties)?));
            values.push(Value::Text(to_json(&edge.provenance)?));
            values.push(
                optional_to_json(&edge.freshness)?
                    .map(Value::Text)
                    .unwrap_or(Value::Null),
            );
            values.push(Value::Text(row_hash(edge)?));
            values.push(
                source_watermark
                    .map(|watermark| Value::Text(watermark.to_string()))
                    .unwrap_or(Value::Null),
            );
        }
        tx.execute(&sql, params_from_iter(values))?;
    }
    Ok(())
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

    pub fn has_user_triggers(&self) -> Result<bool> {
        self.conn
            .query_row(
                r#"
                SELECT EXISTS(
                    SELECT 1
                    FROM sqlite_master
                    WHERE type = 'trigger'
                      AND name NOT LIKE 'sqlite_%'
                )
                "#,
                [],
                |row| row.get::<_, bool>(0),
            )
            .map_err(Into::into)
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
                properties_json TEXT NOT NULL DEFAULT '{}',
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

            CREATE TABLE IF NOT EXISTS graph_node_properties (
                node_id TEXT NOT NULL,
                key TEXT NOT NULL,
                value TEXT NOT NULL,
                PRIMARY KEY (node_id, key),
                FOREIGN KEY (node_id) REFERENCES graph_nodes(id) ON DELETE CASCADE
            );
            CREATE INDEX IF NOT EXISTS idx_graph_node_properties_key_value_node
                ON graph_node_properties(key, value, node_id);

            CREATE TABLE IF NOT EXISTS graph_operator_stats (
                scope TEXT PRIMARY KEY,
                nodes INTEGER NOT NULL,
                edges INTEGER NOT NULL,
                tombstone_nodes INTEGER NOT NULL,
                tombstone_edges INTEGER NOT NULL,
                file_size_bytes INTEGER,
                freelist_bytes INTEGER,
                observed_at_unix INTEGER NOT NULL
            );
            "#,
        )?;
        if user_version < SQLITE_GRAPH_SCHEMA_VERSION {
            migrate_sqlite_graph_schema(&conn, user_version)?;
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
        let observed_at_unix = unix_now();
        let file_size_bytes_before = sqlite_database_size_bytes(&self.conn).ok();
        let force_refresh_writes = self.has_user_triggers().unwrap_or(true);
        let mut phase_timings = Vec::new();

        let tx = self.conn.transaction()?;
        let started = Instant::now();
        tx.execute_batch(
            r#"
            CREATE TEMP TABLE IF NOT EXISTS next_graph_nodes (
                id TEXT PRIMARY KEY,
                kind TEXT NOT NULL,
                label TEXT NOT NULL,
                properties_json TEXT NOT NULL,
                provenance_json TEXT NOT NULL,
                freshness_json TEXT,
                row_hash TEXT NOT NULL,
                source_watermark TEXT
            );
            CREATE TEMP TABLE IF NOT EXISTS next_graph_edges (
                edge_key TEXT PRIMARY KEY,
                from_id TEXT NOT NULL,
                to_id TEXT NOT NULL,
                kind TEXT NOT NULL,
                properties_json TEXT NOT NULL,
                provenance_json TEXT NOT NULL,
                freshness_json TEXT,
                row_hash TEXT NOT NULL,
                source_watermark TEXT
            );
            CREATE INDEX IF NOT EXISTS temp.idx_next_graph_edges_from_to_kind
                ON next_graph_edges(from_id, to_id, kind);
            CREATE TEMP TABLE IF NOT EXISTS next_graph_node_properties (
                node_id TEXT NOT NULL,
                key TEXT NOT NULL,
                value TEXT NOT NULL,
                PRIMARY KEY (node_id, key)
            );
            CREATE TEMP TABLE IF NOT EXISTS next_graph_edge_properties (
                edge_key TEXT NOT NULL,
                key TEXT NOT NULL,
                value TEXT NOT NULL,
                PRIMARY KEY (edge_key, key)
            );
            CREATE TEMP TABLE IF NOT EXISTS next_graph_changed_nodes (
                id TEXT PRIMARY KEY
            );
            CREATE TEMP TABLE IF NOT EXISTS next_graph_changed_edges (
                edge_key TEXT PRIMARY KEY
            );
            DELETE FROM next_graph_nodes;
            DELETE FROM next_graph_edges;
            DELETE FROM next_graph_node_properties;
            DELETE FROM next_graph_edge_properties;
            DELETE FROM next_graph_changed_nodes;
            DELETE FROM next_graph_changed_edges;
            "#,
        )?;
        phase_timings.push(sqlite_refresh_phase_timing(
            "sqlite_temp_table_prepare",
            started,
            "create and clear refresh staging tables before row loading",
        ));
        {
            let started = Instant::now();
            sqlite_stage_projection_nodes(&tx, &projection.nodes, source_watermark.as_deref())?;
            phase_timings.push(sqlite_refresh_phase_timing(
                "sqlite_node_staging",
                started,
                &format!(
                    "bulk stage {} graph_nodes rows into temp table using multi-row chunks up to {} rows before delta comparison",
                    projection.nodes.len(),
                    SQLITE_GRAPH_STAGING_CHUNK_ROWS
                ),
            ));
        }
        {
            let started = Instant::now();
            sqlite_stage_projection_edges(&tx, &projection.edges, source_watermark.as_deref())?;
            phase_timings.push(sqlite_refresh_phase_timing(
                "sqlite_edge_staging",
                started,
                &format!(
                    "bulk stage {} graph_edges rows into temp table using multi-row chunks up to {} rows before delta comparison",
                    projection.edges.len(),
                    SQLITE_GRAPH_STAGING_CHUNK_ROWS
                ),
            ));
        }
        {
            let started = Instant::now();
            let changed_nodes_sql = if force_refresh_writes {
                r#"
                INSERT INTO next_graph_changed_nodes (id)
                SELECT id
                FROM next_graph_nodes
                "#
            } else {
                r#"
                INSERT INTO next_graph_changed_nodes (id)
                SELECT n.id
                FROM next_graph_nodes n
                LEFT JOIN graph_nodes g ON g.id = n.id
                WHERE g.id IS NULL OR g.row_hash IS NOT n.row_hash
                "#
            };
            tx.execute(changed_nodes_sql, [])?;
            tx.execute_batch(
                r#"
                INSERT INTO next_graph_node_properties (node_id, key, value)
                SELECT n.id, json_each.key, CAST(json_each.value AS TEXT)
                FROM next_graph_nodes n
                JOIN next_graph_changed_nodes c ON c.id = n.id,
                     json_each(n.properties_json)
                WHERE json_each.key IS NOT NULL
                  AND json_each.value IS NOT NULL
                "#,
            )?;
            phase_timings.push(sqlite_refresh_phase_timing(
                "sqlite_property_row_staging",
                started,
                "derive materialized node property rows only for new/changed node rows; unchanged row-hash owners reuse existing property rows",
            ));
        }
        {
            let started = Instant::now();
            let changed_edges_sql = if force_refresh_writes {
                r#"
                INSERT INTO next_graph_changed_edges (edge_key)
                SELECT edge_key
                FROM next_graph_edges
                "#
            } else {
                r#"
                INSERT INTO next_graph_changed_edges (edge_key)
                SELECT n.edge_key
                FROM next_graph_edges n
                LEFT JOIN graph_edges g ON g.edge_key = n.edge_key
                WHERE g.edge_key IS NULL OR g.row_hash IS NOT n.row_hash
                "#
            };
            tx.execute(changed_edges_sql, [])?;
            tx.execute_batch(
                r#"
                INSERT INTO next_graph_edge_properties (edge_key, key, value)
                SELECT e.edge_key, json_each.key, CAST(json_each.value AS TEXT)
                FROM next_graph_edges e
                JOIN next_graph_changed_edges c ON c.edge_key = e.edge_key,
                     json_each(e.properties_json)
                WHERE json_each.key IS NOT NULL
                  AND json_each.value IS NOT NULL
                "#,
            )?;
            phase_timings.push(sqlite_refresh_phase_timing(
                "sqlite_edge_property_row_staging",
                started,
                "derive materialized edge property rows only for new/changed edge rows; unchanged row-hash owners reuse existing property rows",
            ));
        }

        let delta_started = Instant::now();
        let tombstoned_nodes = {
            let mut stmt = tx.prepare(
                r#"
                SELECT g.id
                FROM graph_nodes g
                LEFT JOIN next_graph_nodes n ON n.id = g.id
                WHERE n.id IS NULL
                ORDER BY g.id
                "#,
            )?;
            collect_rows(stmt.query_map([], |row| row.get::<_, String>(0))?)?
        };
        let tombstoned_edges = {
            let mut stmt = tx.prepare(
                r#"
                SELECT g.edge_key
                FROM graph_edges g
                LEFT JOIN next_graph_edges n
                    ON n.edge_key = g.edge_key
                WHERE n.edge_key IS NULL
                ORDER BY g.edge_key
                "#,
            )?;
            collect_rows(stmt.query_map([], |row| row.get::<_, String>(0))?)?
        };
        let unchanged_nodes: usize = tx.query_row(
            r#"
            SELECT COUNT(*)
            FROM next_graph_nodes n
            JOIN graph_nodes g ON g.id = n.id
            WHERE g.row_hash = n.row_hash
            "#,
            [],
            |row| row.get(0),
        )?;
        let unchanged_edges: usize = tx.query_row(
            r#"
            SELECT COUNT(*)
            FROM next_graph_edges n
            JOIN graph_edges g
                ON g.edge_key = n.edge_key
            WHERE g.row_hash = n.row_hash
            "#,
            [],
            |row| row.get(0),
        )?;
        let reused_owner_node_properties: usize = tx.query_row(
            r#"
            SELECT COUNT(*)
            FROM graph_node_properties g
            JOIN next_graph_nodes n ON n.id = g.node_id
            LEFT JOIN next_graph_changed_nodes c ON c.id = n.id
            WHERE c.id IS NULL
            "#,
            [],
            |row| row.get(0),
        )?;
        let reused_owner_edge_properties: usize = tx.query_row(
            r#"
            SELECT COUNT(*)
            FROM graph_edge_properties g
            JOIN next_graph_edges n ON n.edge_key = g.edge_key
            LEFT JOIN next_graph_changed_edges c ON c.edge_key = n.edge_key
            WHERE c.edge_key IS NULL
            "#,
            [],
            |row| row.get(0),
        )?;
        let unchanged_changed_node_properties: usize = tx.query_row(
            r#"
            SELECT COUNT(*)
            FROM next_graph_node_properties n
            JOIN graph_node_properties g
                ON g.node_id = n.node_id AND g.key = n.key
            WHERE g.value = n.value
            "#,
            [],
            |row| row.get(0),
        )?;
        let unchanged_changed_edge_properties: usize = tx.query_row(
            r#"
            SELECT COUNT(*)
            FROM next_graph_edge_properties n
            JOIN graph_edge_properties g
                ON g.edge_key = n.edge_key AND g.key = n.key
            WHERE g.value = n.value
            "#,
            [],
            |row| row.get(0),
        )?;
        let unchanged_properties = reused_owner_node_properties
            + reused_owner_edge_properties
            + unchanged_changed_node_properties
            + unchanged_changed_edge_properties;

        let deleted_edges = tx.execute(
            r#"
            DELETE FROM graph_edges
            WHERE NOT EXISTS (
                SELECT 1
                FROM next_graph_edges n
                WHERE n.edge_key = graph_edges.edge_key
            )
            "#,
            [],
        )?;
        let deleted_nodes = tx.execute(
            r#"
            DELETE FROM graph_nodes
            WHERE NOT EXISTS (
                SELECT 1
                FROM next_graph_nodes n
                WHERE n.id = graph_nodes.id
            )
            "#,
            [],
        )?;

        let upsert_nodes_sql = r#"
            INSERT INTO graph_nodes
                (id, kind, label, properties_json, provenance_json, freshness_json, row_hash, source_watermark)
            SELECT
                n.id,
                n.kind,
                n.label,
                n.properties_json,
                n.provenance_json,
                n.freshness_json,
                n.row_hash,
                n.source_watermark
            FROM next_graph_nodes n
            JOIN next_graph_changed_nodes c ON c.id = n.id
            WHERE true
            ON CONFLICT(id) DO UPDATE SET
                kind = excluded.kind,
                label = excluded.label,
                properties_json = excluded.properties_json,
                provenance_json = excluded.provenance_json,
                freshness_json = excluded.freshness_json,
                row_hash = excluded.row_hash,
                source_watermark = excluded.source_watermark
            WHERE graph_nodes.row_hash IS NOT excluded.row_hash
            "#;
        tx.execute(upsert_nodes_sql, [])?;
        let upsert_edges_sql = r#"
            INSERT INTO graph_edges
                (edge_key, from_id, to_id, kind, properties_json, provenance_json, freshness_json, row_hash, source_watermark)
            SELECT
                n.edge_key,
                n.from_id,
                n.to_id,
                n.kind,
                n.properties_json,
                n.provenance_json,
                n.freshness_json,
                n.row_hash,
                n.source_watermark
            FROM next_graph_edges n
            JOIN next_graph_changed_edges c ON c.edge_key = n.edge_key
            WHERE true
            ON CONFLICT(from_id, to_id, kind) DO UPDATE SET
                edge_key = excluded.edge_key,
                properties_json = excluded.properties_json,
                provenance_json = excluded.provenance_json,
                freshness_json = excluded.freshness_json,
                row_hash = excluded.row_hash,
                source_watermark = excluded.source_watermark
            WHERE graph_edges.row_hash IS NOT excluded.row_hash
            "#;
        tx.execute(upsert_edges_sql, [])?;
        let deleted_node_properties = tx.execute(
            r#"
            DELETE FROM graph_node_properties
            WHERE EXISTS (
                SELECT 1
                FROM next_graph_changed_nodes c
                WHERE c.id = graph_node_properties.node_id
            )
              AND NOT EXISTS (
                SELECT 1
                FROM next_graph_node_properties n
                WHERE n.node_id = graph_node_properties.node_id
                  AND n.key = graph_node_properties.key
            )
            "#,
            [],
        )?;
        let deleted_edge_properties = tx.execute(
            r#"
            DELETE FROM graph_edge_properties
            WHERE EXISTS (
                SELECT 1
                FROM next_graph_changed_edges c
                WHERE c.edge_key = graph_edge_properties.edge_key
            )
              AND NOT EXISTS (
                SELECT 1
                FROM next_graph_edge_properties n
                WHERE n.edge_key = graph_edge_properties.edge_key
                  AND n.key = graph_edge_properties.key
            )
            "#,
            [],
        )?;
        let deleted_properties = deleted_node_properties + deleted_edge_properties;
        let upsert_properties_sql = r#"
            INSERT INTO graph_node_properties (node_id, key, value)
            SELECT n.node_id, n.key, n.value
            FROM next_graph_node_properties n
            LEFT JOIN graph_node_properties g
                ON g.node_id = n.node_id AND g.key = n.key
            WHERE g.node_id IS NULL OR g.value IS NOT n.value
            ON CONFLICT(node_id, key) DO UPDATE SET
                value = excluded.value
            WHERE graph_node_properties.value IS NOT excluded.value
            "#;
        let upserted_node_properties = tx.execute(upsert_properties_sql, [])?;
        let upsert_edge_properties_sql = r#"
            INSERT INTO graph_edge_properties (edge_key, key, value)
            SELECT n.edge_key, n.key, n.value
            FROM next_graph_edge_properties n
            LEFT JOIN graph_edge_properties g
                ON g.edge_key = n.edge_key AND g.key = n.key
            WHERE g.edge_key IS NULL OR g.value IS NOT n.value
            ON CONFLICT(edge_key, key) DO UPDATE SET
                value = excluded.value
            WHERE graph_edge_properties.value IS NOT excluded.value
            "#;
        let upserted_edge_properties = tx.execute(upsert_edge_properties_sql, [])?;
        let upserted_properties = upserted_node_properties + upserted_edge_properties;
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
        let pruned_node_tombstones = tx.execute(
            r#"
            DELETE FROM graph_tombstones
            WHERE row_kind = 'node'
              AND EXISTS (
                SELECT 1
                FROM next_graph_nodes n
                WHERE n.id = substr(graph_tombstones.row_key, 6)
              )
            "#,
            [],
        )?;
        let pruned_edge_tombstones = tx.execute(
            r#"
            DELETE FROM graph_tombstones
            WHERE row_kind = 'edge'
              AND EXISTS (
                SELECT 1
                FROM next_graph_edges n
                WHERE n.edge_key = substr(graph_tombstones.row_key, 6)
              )
            "#,
            [],
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
        let tombstone_node_count: usize = tx.query_row(
            "SELECT COUNT(*) FROM graph_tombstones WHERE row_kind = 'node'",
            [],
            |row| row.get(0),
        )?;
        let tombstone_edge_count: usize = tx.query_row(
            "SELECT COUNT(*) FROM graph_tombstones WHERE row_kind = 'edge'",
            [],
            |row| row.get(0),
        )?;
        tx.execute(
            r#"
            INSERT INTO graph_operator_stats
                (scope, nodes, edges, tombstone_nodes, tombstone_edges, file_size_bytes, freelist_bytes, observed_at_unix)
            VALUES (?1, ?2, ?3, ?4, ?5, NULL, NULL, ?6)
            ON CONFLICT(scope) DO UPDATE SET
                nodes = excluded.nodes,
                edges = excluded.edges,
                tombstone_nodes = excluded.tombstone_nodes,
                tombstone_edges = excluded.tombstone_edges,
                file_size_bytes = excluded.file_size_bytes,
                freelist_bytes = excluded.freelist_bytes,
                observed_at_unix = excluded.observed_at_unix
            "#,
            (
                &scope,
                projection.nodes.len() as i64,
                projection.edges.len() as i64,
                tombstone_node_count as i64,
                tombstone_edge_count as i64,
                observed_at_unix,
            ),
        )?;
        phase_timings.push(sqlite_refresh_phase_timing(
            "sqlite_delta_write",
            delta_started,
            "apply row/property deltas, projection metadata, tombstones, and cached operator counts",
        ));
        let commit_started = Instant::now();
        tx.commit()?;
        phase_timings.push(sqlite_refresh_phase_timing(
            "sqlite_commit",
            commit_started,
            "commit refresh transaction and publish old-or-new graph visibility",
        ));
        let file_size_bytes_after = sqlite_database_size_bytes(&self.conn).ok();
        let freelist_bytes_after = sqlite_database_freelist_bytes(&self.conn).ok();
        let stats_started = Instant::now();
        self.conn.execute(
            r#"
            UPDATE graph_operator_stats
            SET file_size_bytes = ?2,
                freelist_bytes = ?3,
                observed_at_unix = ?4
            WHERE scope = ?1
            "#,
            (
                &scope,
                file_size_bytes_after.map(|value| value as i64),
                freelist_bytes_after.map(|value| value as i64),
                unix_now(),
            ),
        )?;
        phase_timings.push(sqlite_refresh_phase_timing(
            "sqlite_stats_cache_update",
            stats_started,
            "persist post-commit file and freelist proof for status/doctor",
        ));
        Ok(SqliteProjectionRefresh {
            scope,
            projection_version,
            source_watermark,
            upserted_nodes: projection.nodes.len().saturating_sub(unchanged_nodes),
            upserted_edges: projection.edges.len().saturating_sub(unchanged_edges),
            unchanged_nodes,
            unchanged_edges,
            upserted_properties,
            unchanged_properties,
            deleted_properties,
            deleted_nodes,
            deleted_edges,
            pruned_tombstones: pruned_node_tombstones + pruned_edge_tombstones,
            file_size_bytes_before,
            file_size_bytes_after,
            tombstoned_nodes,
            tombstoned_edges,
            phase_timings,
        })
    }

    pub fn upsert_projection(&mut self, projection: &GraphProjection) -> Result<()> {
        let tx = self.conn.transaction()?;
        {
            let mut insert_node = tx.prepare(
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
            )?;
            let mut delete_properties =
                tx.prepare("DELETE FROM graph_node_properties WHERE node_id = ?1")?;
            let mut insert_property = tx.prepare(
                r#"
                INSERT INTO graph_node_properties (node_id, key, value)
                VALUES (?1, ?2, ?3)
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
                    row_hash(node)?,
                ))?;
                delete_properties.execute([&node.id])?;
                for (key, value) in &node.properties {
                    insert_property.execute((&node.id, key, value))?;
                }
            }
        }
        {
            let mut insert_edge = tx.prepare(
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
            )?;
            let mut delete_properties =
                tx.prepare("DELETE FROM graph_edge_properties WHERE edge_key = ?1")?;
            let mut insert_property = tx.prepare(
                r#"
                INSERT INTO graph_edge_properties (edge_key, key, value)
                VALUES (?1, ?2, ?3)
                "#,
            )?;
            for edge in &projection.edges {
                let edge_key = graph_edge_id(edge);
                insert_edge.execute((
                    &edge_key,
                    &edge.from_id,
                    &edge.to_id,
                    &edge.kind,
                    to_json(&edge.properties)?,
                    to_json(&edge.provenance)?,
                    optional_to_json(&edge.freshness)?,
                    row_hash(edge)?,
                ))?;
                delete_properties.execute([&edge_key])?;
                for (key, value) in &edge.properties {
                    insert_property.execute((&edge_key, key, value))?;
                }
            }
        }
        tx.commit()?;
        Ok(())
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

    pub fn update_projection_source_watermark(
        &mut self,
        scope: &str,
        source_watermark: Option<String>,
    ) -> Result<()> {
        self.conn.execute(
            r#"
            UPDATE graph_projection_versions
            SET source_watermark = ?2
            WHERE scope = ?1
            "#,
            (scope, source_watermark),
        )?;
        Ok(())
    }

    pub fn compact_storage(&mut self, scope: &str, prune_tombstones: bool) -> Result<usize> {
        let pruned_tombstones = if prune_tombstones {
            self.conn.execute("DELETE FROM graph_tombstones", [])?
        } else {
            0
        };
        self.conn.execute_batch(
            r#"
            PRAGMA wal_checkpoint(TRUNCATE);
            VACUUM;
            "#,
        )?;
        let nodes = self
            .conn
            .query_row("SELECT COUNT(*) FROM graph_nodes", [], |row| {
                row.get::<_, i64>(0)
            })?;
        let edges = self
            .conn
            .query_row("SELECT COUNT(*) FROM graph_edges", [], |row| {
                row.get::<_, i64>(0)
            })?;
        let tombstone_nodes = self.conn.query_row(
            "SELECT COUNT(*) FROM graph_tombstones WHERE row_kind = 'node'",
            [],
            |row| row.get::<_, i64>(0),
        )?;
        let tombstone_edges = self.conn.query_row(
            "SELECT COUNT(*) FROM graph_tombstones WHERE row_kind = 'edge'",
            [],
            |row| row.get::<_, i64>(0),
        )?;
        let file_size_bytes = sqlite_database_size_bytes(&self.conn)
            .ok()
            .map(|value| value as i64);
        let freelist_bytes = sqlite_database_freelist_bytes(&self.conn)
            .ok()
            .map(|value| value as i64);
        self.conn.execute(
            r#"
            INSERT INTO graph_operator_stats
                (scope, nodes, edges, tombstone_nodes, tombstone_edges, file_size_bytes, freelist_bytes, observed_at_unix)
            VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, strftime('%s', 'now'))
            ON CONFLICT(scope) DO UPDATE SET
                nodes = excluded.nodes,
                edges = excluded.edges,
                tombstone_nodes = excluded.tombstone_nodes,
                tombstone_edges = excluded.tombstone_edges,
                file_size_bytes = excluded.file_size_bytes,
                freelist_bytes = excluded.freelist_bytes,
                observed_at_unix = excluded.observed_at_unix
            "#,
            (
                scope,
                nodes,
                edges,
                tombstone_nodes,
                tombstone_edges,
                file_size_bytes,
                freelist_bytes,
            ),
        )?;
        Ok(pruned_tombstones)
    }
}

fn sqlite_query_plan(conn: &Connection, sql: &str, values: &[Value]) -> Result<Vec<String>> {
    let mut stmt = conn.prepare(&format!("EXPLAIN QUERY PLAN {sql}"))?;
    collect_rows(stmt.query_map(params_from_iter(values.iter()), |row| {
        row.get::<_, String>(3)
    })?)
}

fn sqlite_query_plan_diagnostics(plan: &[String], expected_indexes: &[&str]) -> Vec<String> {
    let mut diagnostics = vec![format!(
        "sqlite query pushdown active; plan: {}",
        plan.join(" | ")
    )];
    for expected_index in expected_indexes {
        if plan.iter().any(|row| row.contains(expected_index)) {
            diagnostics.push(format!("sqlite query plan uses {expected_index}"));
        } else {
            diagnostics.push(format!(
                "sqlite query plan did not report {expected_index}; inspect before changing graph property indexes"
            ));
        }
    }
    diagnostics
}

fn push_sqlite_property_filter_exists(
    sql: &mut String,
    values: &mut Vec<Value>,
    node_alias: &str,
    filters: &[GraphPropertyFilter],
) {
    for (index, filter) in filters.iter().enumerate() {
        sql.push_str(&format!(
            r#"
            AND EXISTS (
                SELECT 1
                FROM graph_node_properties p{index} INDEXED BY idx_graph_node_properties_key_value_node
                WHERE p{index}.node_id = {node_alias}.id
                  AND p{index}.key = ?
                  AND p{index}.value = ?
            )
            "#
        ));
        values.push(Value::Text(filter.key.clone()));
        values.push(Value::Text(filter.value.clone()));
    }
}

fn push_sqlite_edge_property_filter_exists(
    sql: &mut String,
    values: &mut Vec<Value>,
    edge_alias: &str,
    filters: &[GraphPropertyFilter],
) {
    for (index, filter) in filters.iter().enumerate() {
        sql.push_str(&format!(
            r#"
            AND EXISTS (
                SELECT 1
                FROM graph_edge_properties ep{index} INDEXED BY idx_graph_edge_properties_key_value_edge
                WHERE ep{index}.edge_key = {edge_alias}.edge_key
                  AND ep{index}.key = ?
                  AND ep{index}.value = ?
            )
            "#
        ));
        values.push(Value::Text(filter.key.clone()));
        values.push(Value::Text(filter.value.clone()));
    }
}

struct SqliteIncidentEdgeBranch<'a> {
    index_name: &'a str,
    endpoint_column: &'a str,
    node_id: &'a str,
    kind: Option<&'a str>,
    filters: &'a [GraphPropertyFilter],
    cursor: Option<&'a str>,
}

fn push_sqlite_incident_edge_branch(
    sql: &mut String,
    values: &mut Vec<Value>,
    branch: SqliteIncidentEdgeBranch<'_>,
) {
    sql.push_str(&format!(
        r#"
        SELECT
            e.edge_key, e.from_id, e.to_id, e.kind, e.properties_json, e.provenance_json, e.freshness_json
        FROM graph_edges e INDEXED BY {index_name}
        WHERE e.{endpoint_column} = ?
        "#,
        index_name = branch.index_name,
        endpoint_column = branch.endpoint_column,
    ));
    values.push(Value::Text(branch.node_id.to_string()));
    if let Some(kind) = branch.kind {
        sql.push_str(" AND e.kind = ?");
        values.push(Value::Text(kind.to_string()));
    }
    push_sqlite_edge_property_filter_exists(sql, values, "e", branch.filters);
    if let Some(cursor) = branch.cursor {
        sql.push_str(" AND e.edge_key > ?");
        values.push(Value::Text(cursor.to_string()));
    }
}

fn sqlite_incident_edges_union_query(
    node_id: &str,
    kind: Option<&str>,
    filters: &[GraphPropertyFilter],
    cursor: Option<&str>,
    limit: Option<usize>,
) -> (String, Vec<Value>) {
    let mut sql = String::from(
        r#"
        SELECT edge_key, from_id, to_id, kind, properties_json, provenance_json, freshness_json
        FROM (
        "#,
    );
    let mut values = Vec::new();
    push_sqlite_incident_edge_branch(
        &mut sql,
        &mut values,
        SqliteIncidentEdgeBranch {
            index_name: "idx_graph_edges_from_kind",
            endpoint_column: "from_id",
            node_id,
            kind,
            filters,
            cursor,
        },
    );
    sql.push_str(" UNION ");
    push_sqlite_incident_edge_branch(
        &mut sql,
        &mut values,
        SqliteIncidentEdgeBranch {
            index_name: "idx_graph_edges_to_kind",
            endpoint_column: "to_id",
            node_id,
            kind,
            filters,
            cursor,
        },
    );
    sql.push_str(
        r#"
        ) e
        ORDER BY e.edge_key
        "#,
    );
    if let Some(limit) = limit {
        sql.push_str(" LIMIT ?");
        values.push(Value::Integer(limit.saturating_add(1) as i64));
    }
    (sql, values)
}

impl GraphStore for SqliteGraphStore {
    fn upsert_node(&self, node: &GraphNode) -> Result<()> {
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
            (
                &node.id,
                &node.kind,
                &node.label,
                to_json(&node.properties)?,
                to_json(&node.provenance)?,
                optional_to_json(&node.freshness)?,
                row_hash(node)?,
            ),
        )?;
        replace_node_properties(&self.conn, &node.id, &node.properties)?;
        Ok(())
    }

    fn upsert_edge(&self, edge: &GraphEdge) -> Result<()> {
        let edge_key = graph_edge_id(edge);
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
            (
                &edge_key,
                &edge.from_id,
                &edge.to_id,
                &edge.kind,
                to_json(&edge.properties)?,
                to_json(&edge.provenance)?,
                optional_to_json(&edge.freshness)?,
                row_hash(edge)?,
            ),
        )?;
        replace_edge_properties(&self.conn, &edge_key, &edge.properties)?;
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
            SELECT edge_key, from_id, to_id, kind, properties_json, provenance_json, freshness_json
            FROM graph_edges
            ORDER BY from_id, kind, to_id
            "#,
        )?;
        collect_rows(stmt.query_map([], edge_from_row)?)
    }

    fn edge(&self, edge_id: &str) -> Result<Option<GraphEdge>> {
        self.conn
            .query_row(
                r#"
                SELECT edge_key, from_id, to_id, kind, properties_json, provenance_json, freshness_json
                FROM graph_edges INDEXED BY idx_graph_edges_edge_key
                WHERE edge_key = ?1
                "#,
                [edge_id],
                edge_from_row,
            )
            .optional()
            .map_err(Into::into)
    }

    fn graph_counts(&self) -> Result<(usize, usize)> {
        let nodes = self
            .conn
            .query_row("SELECT COUNT(*) FROM graph_nodes", [], |row| {
                row.get::<_, usize>(0)
            })?;
        let edges = self
            .conn
            .query_row("SELECT COUNT(*) FROM graph_edges", [], |row| {
                row.get::<_, usize>(0)
            })?;
        Ok((nodes, edges))
    }

    fn sample_edge(&self, kind: Option<&str>) -> Result<Option<GraphEdge>> {
        match kind {
            Some(kind) => self
                .conn
                .query_row(
                    r#"
                    SELECT edge_key, from_id, to_id, kind, properties_json, provenance_json, freshness_json
                    FROM graph_edges INDEXED BY idx_graph_edges_from_kind
                    WHERE from_id <> to_id AND kind = ?1
                    ORDER BY from_id, kind, to_id
                    LIMIT 1
                    "#,
                    [kind],
                    edge_from_row,
                )
                .optional()
                .map_err(Into::into),
            None => self
                .conn
                .query_row(
                    r#"
                    SELECT edge_key, from_id, to_id, kind, properties_json, provenance_json, freshness_json
                    FROM graph_edges INDEXED BY idx_graph_edges_from_kind
                    WHERE from_id <> to_id
                    ORDER BY from_id, kind, to_id
                    LIMIT 1
                    "#,
                    [],
                    edge_from_row,
                )
                .optional()
                .map_err(Into::into),
        }
    }

    fn sample_edge_with_property(&self) -> Result<Option<(GraphEdge, GraphPropertyFilter)>> {
        self.conn
            .query_row(
                r#"
                SELECT e.edge_key, e.from_id, e.to_id, e.kind, e.properties_json, e.provenance_json, e.freshness_json,
                       ep.key, ep.value
                FROM graph_edge_properties ep INDEXED BY idx_graph_edge_properties_key_value_edge
                JOIN graph_edges e INDEXED BY idx_graph_edges_edge_key
                  ON e.edge_key = ep.edge_key
                WHERE e.from_id <> e.to_id
                ORDER BY ep.key, ep.value, ep.edge_key
                LIMIT 1
                "#,
                [],
                |row| {
                    Ok((
                        edge_from_row(row)?,
                        GraphPropertyFilter {
                            key: row.get(7)?,
                            value: row.get(8)?,
                        },
                    ))
                },
            )
            .optional()
            .map_err(Into::into)
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

    fn paged_nodes_by_kind(
        &self,
        kind: &str,
        options: GraphQueryOptions,
    ) -> Result<GraphPagedSubgraph> {
        let mut sql = String::from(
            r#"
            SELECT id, kind, label, properties_json, provenance_json, freshness_json
            FROM graph_nodes
            WHERE kind = ?
            "#,
        );
        let mut values = vec![Value::Text(kind.to_string())];
        push_sqlite_property_filter_exists(
            &mut sql,
            &mut values,
            "graph_nodes",
            &options.property_filters,
        );
        if let Some(cursor) = &options.cursor {
            sql.push_str(" AND id > ?");
            values.push(Value::Text(cursor.clone()));
        }
        sql.push_str(" ORDER BY id");
        if let Some(limit) = options.limit {
            sql.push_str(" LIMIT ?");
            values.push(Value::Integer(limit.saturating_add(1) as i64));
        }

        let plan = sqlite_query_plan(&self.conn, &sql, &values)?;
        let mut stmt = self.conn.prepare(&sql)?;
        let mut nodes =
            collect_rows(stmt.query_map(params_from_iter(values.iter()), node_from_row)?)?;
        let before_limit = nodes.len();
        let mut next_cursor = None;
        if let Some(limit) = options.limit
            && nodes.len() > limit
        {
            next_cursor = nodes
                .get(limit.saturating_sub(1))
                .map(|node| node.id.clone());
            nodes.truncate(limit);
        }
        let expected_indexes = if options.property_filters.is_empty() {
            vec!["idx_graph_nodes_kind"]
        } else {
            vec![
                "idx_graph_nodes_kind",
                "idx_graph_node_properties_key_value_node",
            ]
        };
        let mut diagnostics = sqlite_query_plan_diagnostics(&plan, &expected_indexes);
        if !options.property_filters.is_empty() {
            diagnostics.push(
                "property filters were evaluated by SQLite materialized property rows before paging"
                    .to_string(),
            );
        }
        if options.cursor.is_some() {
            diagnostics.push("cursor is exclusive and pushed into SQLite by node id".to_string());
        }
        if next_cursor.is_some() {
            diagnostics.push(
                "result was truncated; pass page.next_cursor as --cursor for the next page"
                    .to_string(),
            );
        }
        Ok(GraphPagedSubgraph {
            page: GraphQueryPage {
                cursor: options.cursor,
                limit: options.limit,
                next_cursor,
                returned_nodes: nodes.len(),
                returned_edges: 0,
                truncated: options.limit.is_some_and(|limit| before_limit > limit),
                diagnostics,
            },
            nodes,
            edges: Vec::new(),
        })
    }

    fn outgoing_edges(&self, from_id: &str, kind: Option<&str>) -> Result<Vec<GraphEdge>> {
        match kind {
            Some(kind) => {
                let mut stmt = self.conn.prepare(
                    r#"
                    SELECT edge_key, from_id, to_id, kind, properties_json, provenance_json, freshness_json
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
                    SELECT edge_key, from_id, to_id, kind, properties_json, provenance_json, freshness_json
                    FROM graph_edges
                    WHERE from_id = ?1
                    ORDER BY to_id, kind
                    "#,
                )?;
                collect_rows(stmt.query_map([from_id], edge_from_row)?)
            }
        }
    }

    fn incident_edges(&self, node_id: &str, kind: Option<&str>) -> Result<Vec<GraphEdge>> {
        let (sql, values) = sqlite_incident_edges_union_query(node_id, kind, &[], None, None);
        let mut stmt = self.conn.prepare(&sql)?;
        collect_rows(stmt.query_map(params_from_iter(values.iter()), edge_from_row)?)
    }

    fn paged_edges(
        &self,
        kind: Option<&str>,
        options: GraphQueryOptions,
    ) -> Result<GraphPagedSubgraph> {
        let primary_property_filter = options.property_filters.first();
        let mut values = Vec::new();
        let mut sql = if let Some(filter) = primary_property_filter {
            values.push(Value::Text(filter.key.clone()));
            values.push(Value::Text(filter.value.clone()));
            String::from(
                r#"
                SELECT e.edge_key, e.from_id, e.to_id, e.kind, e.properties_json, e.provenance_json, e.freshness_json
                FROM graph_edge_properties ep0 INDEXED BY idx_graph_edge_properties_key_value_edge
                JOIN graph_edges e INDEXED BY idx_graph_edges_edge_key
                  ON e.edge_key = ep0.edge_key
                WHERE ep0.key = ?
                  AND ep0.value = ?
                "#,
            )
        } else {
            String::from(
                r#"
                SELECT edge_key, from_id, to_id, kind, properties_json, provenance_json, freshness_json
                FROM graph_edges e
                WHERE 1 = 1
                "#,
            )
        };
        if let Some(kind) = kind {
            sql.push_str(" AND e.kind = ?");
            values.push(Value::Text(kind.to_string()));
        }
        push_sqlite_edge_property_filter_exists(
            &mut sql,
            &mut values,
            "e",
            if primary_property_filter.is_some() {
                &options.property_filters[1..]
            } else {
                &options.property_filters
            },
        );
        if let Some(cursor) = &options.cursor {
            if primary_property_filter.is_some() {
                sql.push_str(" AND ep0.edge_key > ?");
            } else {
                sql.push_str(" AND e.edge_key > ?");
            }
            values.push(Value::Text(cursor.clone()));
        }
        if primary_property_filter.is_some() {
            sql.push_str(" ORDER BY ep0.edge_key");
        } else {
            sql.push_str(" ORDER BY e.edge_key");
        }
        if let Some(limit) = options.limit {
            sql.push_str(" LIMIT ?");
            values.push(Value::Integer(limit.saturating_add(1) as i64));
        }

        let primary_property_row_count = if let Some(filter) = primary_property_filter {
            Some(self.conn.query_row(
                r#"
                SELECT COUNT(*)
                FROM graph_edge_properties INDEXED BY idx_graph_edge_properties_key_value_edge
                WHERE key = ?1 AND value = ?2
                "#,
                (&filter.key, &filter.value),
                |row| row.get::<_, usize>(0),
            )?)
        } else {
            None
        };
        let plan = sqlite_query_plan(&self.conn, &sql, &values)?;
        let mut stmt = self.conn.prepare(&sql)?;
        let mut edges =
            collect_rows(stmt.query_map(params_from_iter(values.iter()), edge_from_row)?)?;
        let before_limit = edges.len();
        let mut next_cursor = None;
        if let Some(limit) = options.limit
            && edges.len() > limit
        {
            next_cursor = edges.get(limit.saturating_sub(1)).map(graph_edge_id);
            edges.truncate(limit);
        }
        let expected_indexes = if options.property_filters.is_empty() {
            vec!["idx_graph_edges_edge_key"]
        } else {
            vec![
                "idx_graph_edge_properties_key_value_edge",
                "idx_graph_edges_edge_key",
            ]
        };
        let mut diagnostics = sqlite_query_plan_diagnostics(&plan, &expected_indexes);
        if !options.property_filters.is_empty() {
            if let Some(row_count) = primary_property_row_count {
                diagnostics.push(format!(
                    "edge property primary filter matched {row_count} materialized row(s) before edge-kind/cursor paging"
                ));
            }
            diagnostics.push(
                "edge property scan drives from SQLite materialized property rows before joining graph_edges"
                    .to_string(),
            );
        }
        if options.cursor.is_some() {
            diagnostics.push("cursor is exclusive and pushed into SQLite by edge id".to_string());
        }
        if next_cursor.is_some() {
            diagnostics.push(
                "result was truncated; pass page.next_cursor as --cursor for the next page"
                    .to_string(),
            );
        }
        Ok(GraphPagedSubgraph {
            page: GraphQueryPage {
                cursor: options.cursor,
                limit: options.limit,
                next_cursor,
                returned_nodes: 0,
                returned_edges: edges.len(),
                truncated: options.limit.is_some_and(|limit| before_limit > limit),
                diagnostics,
            },
            nodes: Vec::new(),
            edges,
        })
    }

    fn paged_incident_edges(
        &self,
        node_id: &str,
        kind: Option<&str>,
        options: GraphQueryOptions,
    ) -> Result<GraphPagedSubgraph> {
        let (sql, values) = sqlite_incident_edges_union_query(
            node_id,
            kind,
            &options.property_filters,
            options.cursor.as_deref(),
            options.limit,
        );
        let plan = sqlite_query_plan(&self.conn, &sql, &values)?;
        let mut stmt = self.conn.prepare(&sql)?;
        let mut edges =
            collect_rows(stmt.query_map(params_from_iter(values.iter()), edge_from_row)?)?;
        let before_limit = edges.len();
        let mut next_cursor = None;
        if let Some(limit) = options.limit
            && edges.len() > limit
        {
            next_cursor = edges.get(limit.saturating_sub(1)).map(graph_edge_id);
            edges.truncate(limit);
        }
        let expected_indexes = if options.property_filters.is_empty() {
            vec!["idx_graph_edges_from_kind", "idx_graph_edges_to_kind"]
        } else {
            vec![
                "idx_graph_edges_from_kind",
                "idx_graph_edges_to_kind",
                "idx_graph_edge_properties_key_value_edge",
            ]
        };
        let mut diagnostics = sqlite_query_plan_diagnostics(&plan, &expected_indexes);
        diagnostics.push(
            "incident edge scan uses UNION over from_id/to_id index probes instead of an OR predicate"
                .to_string(),
        );
        if !options.property_filters.is_empty() {
            diagnostics.push(
                "edge property filters were evaluated by SQLite materialized property rows before paging"
                    .to_string(),
            );
        }
        if options.cursor.is_some() {
            diagnostics.push("cursor is exclusive and pushed into SQLite by edge id".to_string());
        }
        if next_cursor.is_some() {
            diagnostics.push(
                "result was truncated; pass page.next_cursor as --cursor for the next page"
                    .to_string(),
            );
        }
        Ok(GraphPagedSubgraph {
            page: GraphQueryPage {
                cursor: options.cursor,
                limit: options.limit,
                next_cursor,
                returned_nodes: 0,
                returned_edges: edges.len(),
                truncated: options.limit.is_some_and(|limit| before_limit > limit),
                diagnostics,
            },
            nodes: Vec::new(),
            edges,
        })
    }

    fn edges_between_nodes(&self, node_ids: &BTreeSet<String>) -> Result<Vec<GraphEdge>> {
        if node_ids.is_empty() {
            return Ok(Vec::new());
        }
        let node_ids = node_ids.iter().cloned().collect::<Vec<_>>();
        let mut edges = BTreeMap::<(String, String, String), GraphEdge>::new();
        for from_chunk in node_ids.chunks(450) {
            let from_placeholders = std::iter::repeat_n("?", from_chunk.len())
                .collect::<Vec<_>>()
                .join(", ");
            for to_chunk in node_ids.chunks(450) {
                let to_placeholders = std::iter::repeat_n("?", to_chunk.len())
                    .collect::<Vec<_>>()
                    .join(", ");
                let sql = format!(
                    r#"
                    SELECT edge_key, from_id, to_id, kind, properties_json, provenance_json, freshness_json
                    FROM graph_edges INDEXED BY idx_graph_edges_from_kind
                    WHERE from_id IN ({from_placeholders})
                      AND to_id IN ({to_placeholders})
                    ORDER BY from_id, kind, to_id
                    "#
                );
                let mut values = from_chunk
                    .iter()
                    .cloned()
                    .map(Value::Text)
                    .collect::<Vec<_>>();
                values.extend(to_chunk.iter().cloned().map(Value::Text));
                let mut stmt = self.conn.prepare(&sql)?;
                for edge in
                    collect_rows(stmt.query_map(params_from_iter(values.iter()), edge_from_row)?)?
                {
                    edges
                        .entry((edge.from_id.clone(), edge.kind.clone(), edge.to_id.clone()))
                        .or_insert(edge);
                }
            }
        }
        Ok(edges.into_values().collect())
    }

    fn neighborhood(
        &self,
        center_id: &str,
        depth: usize,
        kind: Option<&str>,
    ) -> Result<Option<GraphSubgraph>> {
        self.paged_neighborhood(center_id, depth, kind, GraphQueryOptions::default())
            .map(|result| {
                result.map(|result| {
                    GraphSubgraph {
                        nodes: result.nodes,
                        edges: result.edges,
                    }
                    .sorted()
                })
            })
    }

    fn paged_neighborhood(
        &self,
        center_id: &str,
        depth: usize,
        kind: Option<&str>,
        options: GraphQueryOptions,
    ) -> Result<Option<GraphPagedSubgraph>> {
        if self.node(center_id)?.is_none() {
            return Ok(None);
        }
        let mut sql = String::from(
            r#"
            WITH RECURSIVE walk(id, depth) AS (
                SELECT ?, 0
                UNION
                SELECT e.to_id, walk.depth + 1
                FROM walk
                JOIN graph_edges e INDEXED BY idx_graph_edges_from_kind
                    ON e.from_id = walk.id
                WHERE walk.depth < ?
            "#,
        );
        let mut values = vec![
            Value::Text(center_id.to_string()),
            Value::Integer(depth as i64),
        ];
        if let Some(kind) = kind {
            sql.push_str(" AND e.kind = ?");
            values.push(Value::Text(kind.to_string()));
        }
        sql.push_str(
            r#"
            ),
            filtered_nodes AS (
            SELECT DISTINCT n.id, n.kind, n.label, n.properties_json, n.provenance_json, n.freshness_json
            FROM walk
            JOIN graph_nodes n ON n.id = walk.id
            WHERE 1 = 1
            "#,
        );
        push_sqlite_property_filter_exists(&mut sql, &mut values, "n", &options.property_filters);
        if let Some(cursor) = &options.cursor {
            sql.push_str(" AND n.id > ?");
            values.push(Value::Text(cursor.clone()));
        }
        sql.push_str(
            r#"
            ),
            page_nodes AS (
                SELECT id, kind, label, properties_json, provenance_json, freshness_json
                FROM filtered_nodes
                ORDER BY id
            "#,
        );
        if let Some(limit) = options.limit {
            sql.push_str(" LIMIT ?");
            values.push(Value::Integer(limit.saturating_add(1) as i64));
        }
        sql.push_str(
            r#"
            ),
            walk_edges AS (
                SELECT e.edge_key, e.from_id, e.to_id, e.kind, e.properties_json, e.provenance_json, e.freshness_json
                FROM walk
                JOIN graph_edges e INDEXED BY idx_graph_edges_from_kind
                    ON e.from_id = walk.id
                WHERE walk.depth < ?
            "#,
        );
        values.push(Value::Integer(depth as i64));
        if let Some(kind) = kind {
            sql.push_str(" AND e.kind = ?");
            values.push(Value::Text(kind.to_string()));
        }
        sql.push_str(
            r#"
            )
            SELECT
                'node' AS row_type,
                p.id, p.kind, p.label, p.properties_json, p.provenance_json, p.freshness_json,
                NULL AS edge_key, NULL AS from_id, NULL AS to_id, NULL AS edge_kind,
                NULL AS edge_properties_json, NULL AS edge_provenance_json, NULL AS edge_freshness_json
            FROM page_nodes p
            UNION ALL
            SELECT DISTINCT
                'edge' AS row_type,
                NULL AS id, NULL AS kind, NULL AS label, NULL AS properties_json,
                NULL AS provenance_json, NULL AS freshness_json,
                e.edge_key, e.from_id, e.to_id, e.kind, e.properties_json, e.provenance_json, e.freshness_json
            FROM walk_edges e
            WHERE e.from_id IN (SELECT id FROM page_nodes)
              AND e.to_id IN (SELECT id FROM page_nodes)
            "#,
        );

        let plan = sqlite_query_plan(&self.conn, &sql, &values)?;
        let mut stmt = self.conn.prepare(&sql)?;
        let mut nodes = Vec::new();
        let mut edges = Vec::new();
        let rows = stmt.query_map(params_from_iter(values.iter()), |row| {
            let row_type: String = row.get(0)?;
            match row_type.as_str() {
                "node" => Ok((Some(node_from_row_at(row, 1)?), None)),
                "edge" => Ok((None, Some(edge_from_row_at(row, 7)?))),
                _ => Err(rusqlite::Error::InvalidQuery),
            }
        })?;
        for row in rows {
            let (node, edge) = row?;
            if let Some(node) = node {
                nodes.push(node);
            }
            if let Some(edge) = edge {
                edges.push(edge);
            }
        }
        nodes.sort_by(|left, right| left.id.cmp(&right.id));
        let before_limit = nodes.len();
        let mut next_cursor = None;
        if let Some(limit) = options.limit
            && nodes.len() > limit
        {
            next_cursor = nodes
                .get(limit.saturating_sub(1))
                .map(|node| node.id.clone());
            nodes.truncate(limit);
        }
        let node_ids = nodes
            .iter()
            .map(|node| node.id.as_str())
            .collect::<BTreeSet<_>>();
        edges.retain(|edge| {
            node_ids.contains(edge.from_id.as_str()) && node_ids.contains(edge.to_id.as_str())
        });
        edges.sort_by(|left, right| {
            left.from_id
                .cmp(&right.from_id)
                .then(left.kind.cmp(&right.kind))
                .then(left.to_id.cmp(&right.to_id))
        });
        let expected_indexes = if options.property_filters.is_empty() {
            vec!["idx_graph_edges_from_kind"]
        } else {
            vec![
                "idx_graph_edges_from_kind",
                "idx_graph_node_properties_key_value_node",
            ]
        };
        let mut diagnostics = sqlite_query_plan_diagnostics(&plan, &expected_indexes);
        diagnostics.push(
            "neighborhood nodes and page edges share one recursive reachable-set CTE".to_string(),
        );
        if !options.property_filters.is_empty() {
            diagnostics.push(
                "property filters were evaluated by SQLite materialized property rows before paging"
                    .to_string(),
            );
        }
        if options.cursor.is_some() {
            diagnostics.push("cursor is exclusive and pushed into SQLite by node id".to_string());
        }
        if next_cursor.is_some() {
            diagnostics.push(
                "result was truncated; pass page.next_cursor as --cursor for the next page"
                    .to_string(),
            );
        }
        Ok(Some(GraphPagedSubgraph {
            page: GraphQueryPage {
                cursor: options.cursor,
                limit: options.limit,
                next_cursor,
                returned_nodes: nodes.len(),
                returned_edges: edges.len(),
                truncated: options.limit.is_some_and(|limit| before_limit > limit),
                diagnostics,
            },
            nodes,
            edges,
        }))
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
        let mut single_frontier_stmt = if kind.is_none() {
            Some(self.conn.prepare(
                r#"
                SELECT from_id, to_id
                FROM graph_edges INDEXED BY idx_graph_edges_from_kind
                WHERE from_id = ?1
                ORDER BY from_id, to_id, kind
                "#,
            )?)
        } else {
            None
        };
        let mut single_frontier_kind_stmt = if kind.is_some() {
            Some(self.conn.prepare(
                r#"
                SELECT from_id, to_id
                FROM graph_edges INDEXED BY idx_graph_edges_from_kind
                WHERE from_id = ?1 AND kind = ?2
                ORDER BY from_id, to_id, kind
                "#,
            )?)
        } else {
            None
        };
        for _depth in 0..hop_limit {
            if frontier.is_empty() {
                break;
            }
            let mut next_frontier = BTreeSet::new();
            for chunk in frontier.chunks(256) {
                let edges = if chunk.len() == 1 {
                    match kind {
                        Some(kind) => {
                            let stmt = single_frontier_kind_stmt
                                .as_mut()
                                .context("single-frontier kind statement missing")?;
                            collect_rows(stmt.query_map((chunk[0].as_str(), kind), |row| {
                                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
                            })?)?
                        }
                        None => {
                            let stmt = single_frontier_stmt
                                .as_mut()
                                .context("single-frontier statement missing")?;
                            collect_rows(stmt.query_map([chunk[0].as_str()], |row| {
                                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
                            })?)?
                        }
                    }
                } else {
                    let placeholders = std::iter::repeat_n("?", chunk.len())
                        .collect::<Vec<_>>()
                        .join(", ");
                    let mut sql = format!(
                        r#"
                        SELECT from_id, to_id
                        FROM graph_edges INDEXED BY idx_graph_edges_from_kind
                        WHERE from_id IN ({placeholders})
                        "#
                    );
                    let mut values = chunk.iter().cloned().map(Value::Text).collect::<Vec<_>>();
                    if let Some(kind) = kind {
                        sql.push_str(" AND kind = ?");
                        values.push(Value::Text(kind.to_string()));
                    }
                    sql.push_str(" ORDER BY from_id, to_id, kind");
                    let mut stmt = self.conn.prepare(&sql)?;
                    collect_rows(stmt.query_map(params_from_iter(values.iter()), |row| {
                        Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
                    })?)?
                };
                for (from, next) in edges {
                    if !visited.insert(next.clone()) {
                        continue;
                    }
                    parent.insert(next.clone(), from);
                    if next == to_id {
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
                    next_frontier.insert(next);
                }
            }
            frontier = next_frontier.into_iter().collect();
        }
        Ok(None)
    }

    fn reachable_nodes_by_kind(
        &self,
        from_id: &str,
        kind: &str,
        depth: usize,
        limit: usize,
    ) -> Result<Vec<(GraphNode, GraphPath)>> {
        Ok(self
            .reachable_nodes_by_kinds(from_id, &[kind], depth, limit)?
            .remove(kind)
            .unwrap_or_default())
    }

    fn reachable_nodes_by_kinds(
        &self,
        from_id: &str,
        kinds: &[&str],
        depth: usize,
        limit: usize,
    ) -> Result<BTreeMap<String, Vec<(GraphNode, GraphPath)>>> {
        let mut requested = kinds
            .iter()
            .map(|kind| (*kind).to_string())
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect::<Vec<_>>();
        let mut results = requested
            .iter()
            .map(|kind| (kind.clone(), Vec::new()))
            .collect::<BTreeMap<_, _>>();
        if requested.is_empty() {
            return Ok(results);
        }
        requested.sort();
        let placeholders = std::iter::repeat_n("?", requested.len())
            .collect::<Vec<_>>()
            .join(", ");
        let mut sql = format!(
            r#"
            WITH RECURSIVE walk(id, depth, path) AS (
                SELECT ?, 0, char(31) || ? || char(31)
                UNION ALL
                SELECT e.to_id, walk.depth + 1, walk.path || e.to_id || char(31)
                FROM walk
                JOIN graph_edges e INDEXED BY idx_graph_edges_from_kind
                    ON e.from_id = walk.id
                WHERE walk.depth < ?
                  AND instr(walk.path, char(31) || e.to_id || char(31)) = 0
            ),
            ranked AS (
                SELECT
                    n.id, n.kind, n.label, n.properties_json, n.provenance_json, n.freshness_json,
                    walk.path, walk.depth,
                    ROW_NUMBER() OVER (PARTITION BY n.kind, n.id ORDER BY walk.depth, n.label, n.id) AS rn
                FROM walk
                JOIN graph_nodes n ON n.id = walk.id
                WHERE n.kind IN ({placeholders}) AND n.id <> ?
            ),
            kind_ranked AS (
                SELECT *,
                    ROW_NUMBER() OVER (PARTITION BY kind ORDER BY depth, label, id) AS kind_rank
                FROM ranked
                WHERE rn = 1
            )
            SELECT id, kind, label, properties_json, provenance_json, freshness_json, path, depth
            FROM kind_ranked
            "#,
        );
        let mut values = vec![
            Value::Text(from_id.to_string()),
            Value::Text(from_id.to_string()),
            Value::Integer(depth as i64),
        ];
        values.extend(requested.iter().cloned().map(Value::Text));
        values.push(Value::Text(from_id.to_string()));
        if limit > 0 && limit != usize::MAX {
            sql.push_str(" WHERE kind_rank <= ?");
            values.push(Value::Integer(limit as i64));
        }
        sql.push_str(" ORDER BY kind, depth, label, id");
        let mut stmt = self.conn.prepare(&sql)?;
        let rows = collect_rows(stmt.query_map(params_from_iter(values.iter()), |row| {
            let node = node_from_row(row)?;
            let path: String = row.get(6)?;
            let hops: usize = row.get(7)?;
            Ok((
                node,
                GraphPath {
                    nodes: path
                        .split('\u{1f}')
                        .filter(|part| !part.is_empty())
                        .map(str::to_string)
                        .collect(),
                    hops,
                },
            ))
        })?)?;
        for (node, path) in rows {
            results
                .entry(node.kind.clone())
                .or_default()
                .push((node, path));
        }
        Ok(results)
    }

    fn resolve_evidence_target(&self, target: &str, kinds: &[&str]) -> Result<Option<GraphNode>> {
        if let Some(node) = self.node(target)? {
            return Ok(Some(node));
        }
        if kinds.is_empty() {
            return Ok(None);
        }

        let normalized = target.trim().trim_start_matches('#');
        let kind_placeholders = std::iter::repeat_n("?", kinds.len())
            .collect::<Vec<_>>()
            .join(", ");
        let kind_rank = kinds
            .iter()
            .enumerate()
            .map(|(rank, _)| format!("WHEN ? THEN {rank}"))
            .collect::<Vec<_>>()
            .join(" ");
        let sql = format!(
            r#"
            SELECT n.id, n.kind, n.label, n.properties_json, n.provenance_json, n.freshness_json
            FROM graph_nodes n
            WHERE n.kind IN ({kind_placeholders})
              AND (
                EXISTS (
                    SELECT 1
                    FROM graph_node_properties p_handle INDEXED BY idx_graph_node_properties_key_value_node
                    WHERE p_handle.node_id = n.id
                      AND p_handle.key = 'handle'
                      AND p_handle.value = ?
                )
                OR EXISTS (
                    SELECT 1
                    FROM graph_node_properties p_ref INDEXED BY idx_graph_node_properties_key_value_node
                    WHERE p_ref.node_id = n.id
                      AND p_ref.key = 'ref_id'
                      AND p_ref.value = ?
                )
                OR n.label = ?
                OR n.label = ?
              )
            ORDER BY CASE n.kind {kind_rank} ELSE 999 END, n.id
            LIMIT 1
            "#
        );
        let mut values = kinds
            .iter()
            .map(|kind| Value::Text((*kind).to_string()))
            .collect::<Vec<_>>();
        values.push(Value::Text(target.to_string()));
        values.push(Value::Text(normalized.to_string()));
        values.push(Value::Text(target.to_string()));
        values.push(Value::Text(format!("#{normalized}")));
        values.extend(kinds.iter().map(|kind| Value::Text((*kind).to_string())));
        self.conn
            .query_row(&sql, params_from_iter(values.iter()), node_from_row)
            .optional()
            .map_err(Into::into)
    }
}

fn to_json<T: Serialize>(value: &T) -> Result<String> {
    serde_json::to_string(value).map_err(Into::into)
}

fn row_hash<T: Serialize>(value: &T) -> Result<String> {
    let payload = serde_json::to_vec(value)?;
    Ok(blake3::hash(&payload).to_hex().to_string())
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

fn node_from_row_at(row: &Row<'_>, offset: usize) -> rusqlite::Result<GraphNode> {
    let properties_col = offset + 3;
    let provenance_col = offset + 4;
    let freshness_col = offset + 5;
    let properties_json: String = row.get(properties_col)?;
    let provenance_json: String = row.get(provenance_col)?;
    let freshness_json: Option<String> = row.get(freshness_col)?;
    Ok(GraphNode {
        id: row.get(offset)?,
        kind: row.get(offset + 1)?,
        label: row.get(offset + 2)?,
        properties: from_json(properties_col, &properties_json)?,
        provenance: from_json(provenance_col, &provenance_json)?,
        freshness: optional_from_json(freshness_col, freshness_json)?,
    })
}

fn node_from_row(row: &Row<'_>) -> rusqlite::Result<GraphNode> {
    node_from_row_at(row, 0)
}

fn edge_from_row_at(row: &Row<'_>, offset: usize) -> rusqlite::Result<GraphEdge> {
    let properties_col = offset + 4;
    let provenance_col = offset + 5;
    let freshness_col = offset + 6;
    let properties_json: String = row.get(properties_col)?;
    let provenance_json: String = row.get(provenance_col)?;
    let freshness_json: Option<String> = row.get(freshness_col)?;
    Ok(GraphEdge {
        id: row.get(offset)?,
        from_id: row.get(offset + 1)?,
        to_id: row.get(offset + 2)?,
        kind: row.get(offset + 3)?,
        properties: from_json(properties_col, &properties_json)?,
        provenance: from_json(provenance_col, &provenance_json)?,
        freshness: optional_from_json(freshness_col, freshness_json)?,
    })
}

fn edge_from_row(row: &Row<'_>) -> rusqlite::Result<GraphEdge> {
    edge_from_row_at(row, 0)
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

fn sqlite_database_size_bytes(conn: &Connection) -> Result<u64> {
    let page_count: u64 = conn.query_row("PRAGMA page_count", [], |row| row.get(0))?;
    let page_size: u64 = conn.query_row("PRAGMA page_size", [], |row| row.get(0))?;
    Ok(page_count.saturating_mul(page_size))
}

fn sqlite_database_freelist_bytes(conn: &Connection) -> Result<u64> {
    let freelist_count: u64 = conn.query_row("PRAGMA freelist_count", [], |row| row.get(0))?;
    let page_size: u64 = conn.query_row("PRAGMA page_size", [], |row| row.get(0))?;
    Ok(freelist_count.saturating_mul(page_size))
}

#[cfg(test)]
mod tests {
    use super::*;

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
    fn sqlite_materializes_edge_properties_and_scans_first_class_edges() {
        let store = SqliteGraphStore::in_memory().unwrap();
        for node in [
            GraphNode::new("doc:livekit", "document", "LiveKit guide"),
            GraphNode::new("topic:rooms", "topic", "Rooms"),
            GraphNode::new("topic:egress", "topic", "Egress"),
        ] {
            store.upsert_node(&node).unwrap();
        }
        let edge = GraphEdge::new("doc:livekit", "topic:rooms", "mentions")
            .with_property("confidence", "0.91");
        let edge_id = edge.id.clone();
        store.upsert_edge(&edge).unwrap();
        store
            .upsert_edge(
                &GraphEdge::new("topic:egress", "topic:rooms", "related_to")
                    .with_property("confidence", "0.42"),
            )
            .unwrap();

        assert_eq!(store.edge(&edge_id).unwrap(), Some(edge));
        let mut expected_incident_ids = vec![
            GraphEdge::stable_id("doc:livekit", "topic:rooms", "mentions"),
            GraphEdge::stable_id("topic:egress", "topic:rooms", "related_to"),
        ];
        expected_incident_ids.sort();
        assert_eq!(
            store
                .incident_edges("topic:rooms", None)
                .unwrap()
                .into_iter()
                .map(|edge| edge.id)
                .collect::<Vec<_>>(),
            expected_incident_ids
        );

        let page = store
            .paged_edges(
                Some("mentions"),
                GraphQueryOptions {
                    property_filters: vec![GraphPropertyFilter {
                        key: "confidence".to_string(),
                        value: "0.91".to_string(),
                    }],
                    ..GraphQueryOptions::default()
                },
            )
            .unwrap();
        assert_eq!(page.edges.len(), 1);
        assert_eq!(page.edges[0].id, edge_id);
        assert!(
            page.page
                .diagnostics
                .iter()
                .any(|diagnostic| diagnostic.contains("idx_graph_edge_properties_key_value_edge")),
            "{:?}",
            page.page.diagnostics
        );
        assert!(
            page.page
                .diagnostics
                .iter()
                .any(|diagnostic| diagnostic.contains("idx_graph_edges_edge_key")),
            "{:?}",
            page.page.diagnostics
        );
        assert!(
            page.page.diagnostics.iter().any(|diagnostic| diagnostic
                .contains("edge property primary filter matched 1 materialized row")),
            "{:?}",
            page.page.diagnostics
        );
        assert!(
            page.page
                .diagnostics
                .iter()
                .any(|diagnostic| diagnostic
                    .contains("drives from SQLite materialized property rows")),
            "{:?}",
            page.page.diagnostics
        );

        let property_rows: usize = store
            .conn
            .query_row(
                "SELECT COUNT(*) FROM graph_edge_properties WHERE key = 'confidence'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(property_rows, 2);
    }

    #[test]
    fn graph_projection_round_trips_through_backend_agnostic_store_contract() {
        let sqlite = SqliteGraphStore::in_memory().unwrap();
        assert_projection_store_contract(&sqlite);
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
    }

    #[test]
    fn sqlite_upsert_projection_batches_rows_and_properties() {
        let mut store = SqliteGraphStore::in_memory().unwrap();
        let mut projection = sample_projection();
        store.upsert_projection(&projection).unwrap();

        let page = store
            .paged_nodes_by_kind(
                "document",
                GraphQueryOptions {
                    property_filters: vec![GraphPropertyFilter {
                        key: "domain".to_string(),
                        value: "livekit".to_string(),
                    }],
                    ..GraphQueryOptions::default()
                },
            )
            .unwrap();
        assert_eq!(page.nodes[0].id, "doc:livekit");

        projection.nodes[0] = GraphNode::new("doc:livekit", "document", "LiveKit guide")
            .with_property("domain", "recording");
        store.upsert_projection(&projection).unwrap();

        let old_property_count: usize = store
            .conn
            .query_row(
                "SELECT COUNT(*) FROM graph_node_properties WHERE key = 'domain' AND value = 'livekit'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        let updated_page = store
            .paged_nodes_by_kind(
                "document",
                GraphQueryOptions {
                    property_filters: vec![GraphPropertyFilter {
                        key: "domain".to_string(),
                        value: "recording".to_string(),
                    }],
                    ..GraphQueryOptions::default()
                },
            )
            .unwrap();
        assert_eq!(old_property_count, 0);
        assert_eq!(updated_page.nodes[0].id, "doc:livekit");
        let edge_property_count: usize = store
            .conn
            .query_row(
                "SELECT COUNT(*) FROM graph_edge_properties WHERE key = 'confidence'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(edge_property_count, 1);
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
        assert_eq!(store.graph_counts().unwrap(), (3, 3));
        assert_eq!(
            store.sample_edge(Some("calls")).unwrap().unwrap().to_id,
            "b"
        );

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
    fn sqlite_store_batches_edges_between_node_sets() {
        let store = SqliteGraphStore::in_memory().unwrap();
        for id in ["a", "b", "c", "outside"] {
            store
                .upsert_node(&GraphNode::new(id, "symbol", id))
                .unwrap();
        }
        for edge in [
            GraphEdge::new("a", "b", "calls"),
            GraphEdge::new("b", "c", "calls"),
            GraphEdge::new("a", "outside", "calls"),
            GraphEdge::new("outside", "c", "calls"),
        ] {
            store.upsert_edge(&edge).unwrap();
        }

        let scoped = ["a".to_string(), "b".to_string(), "c".to_string()]
            .into_iter()
            .collect::<BTreeSet<_>>();
        let edge_keys = store
            .edges_between_nodes(&scoped)
            .unwrap()
            .into_iter()
            .map(|edge| (edge.from_id, edge.kind, edge.to_id))
            .collect::<Vec<_>>();

        assert_eq!(
            edge_keys,
            vec![
                ("a".to_string(), "calls".to_string(), "b".to_string()),
                ("b".to_string(), "calls".to_string(), "c".to_string()),
            ]
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
        assert_eq!(refresh.deleted_nodes, 1);
        assert_eq!(refresh.deleted_edges, 1);
        assert_eq!(refresh.unchanged_nodes, 3);
        assert_eq!(refresh.upserted_nodes, 0);
        assert_eq!(refresh.unchanged_properties, 4);
        assert_eq!(refresh.upserted_properties, 0);
        assert_eq!(refresh.deleted_properties, 0);
        assert!(
            refresh
                .phase_timings
                .iter()
                .any(|phase| phase.name == "sqlite_property_row_staging"),
            "{:?}",
            refresh.phase_timings
        );
        assert!(
            refresh
                .phase_timings
                .iter()
                .any(|phase| phase.name == "sqlite_edge_property_row_staging"),
            "{:?}",
            refresh.phase_timings
        );
        let version = store.projection_version("root").unwrap().unwrap();
        assert_eq!(version.projection_version, "fixture-v2");
        assert_eq!(version.source_watermark.as_deref(), Some("commit-b"));
        let cached_counts: (usize, usize, usize, usize) = store
            .conn
            .query_row(
                r#"
                SELECT nodes, edges, tombstone_nodes, tombstone_edges
                FROM graph_operator_stats
                WHERE scope = 'root'
                "#,
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
            )
            .unwrap();
        assert_eq!(cached_counts, (3, 1, 1, 1));

        projection
            .nodes
            .push(GraphNode::new("topic:egress", "topic", "Egress"));
        let refresh = store
            .replace_projection_with_version(
                "root",
                &projection,
                Some("fixture-v3"),
                Some("commit-c".to_string()),
            )
            .unwrap();
        assert_eq!(refresh.pruned_tombstones, 1);
        assert_eq!(refresh.tombstoned_nodes, Vec::<String>::new());

        projection.nodes.retain(|node| node.id != "topic:egress");
        store
            .replace_projection_with_version(
                "root",
                &projection,
                Some("fixture-v4"),
                Some("commit-d".to_string()),
            )
            .unwrap();
        assert_eq!(store.compact_storage("root", true).unwrap(), 2);
        let cached_counts: (usize, usize, usize, usize) = store
            .conn
            .query_row(
                r#"
                SELECT nodes, edges, tombstone_nodes, tombstone_edges
                FROM graph_operator_stats
                WHERE scope = 'root'
                "#,
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
            )
            .unwrap();
        assert_eq!(cached_counts, (3, 1, 0, 0));
    }

    #[test]
    fn sqlite_shortest_path_uses_bounded_frontier() {
        let store = SqliteGraphStore::in_memory().unwrap();
        for idx in 0..80 {
            store
                .upsert_node(&GraphNode::new(
                    format!("node:{idx:02}"),
                    "symbol",
                    format!("node {idx:02}"),
                ))
                .unwrap();
        }
        for idx in 0..79 {
            store
                .upsert_edge(&GraphEdge::new(
                    format!("node:{idx:02}"),
                    format!("node:{:02}", idx + 1),
                    "calls",
                ))
                .unwrap();
        }
        store
            .upsert_edge(&GraphEdge::new("node:00", "node:79", "mentions"))
            .unwrap();

        assert!(
            store
                .shortest_path_with_max_hops("node:00", "node:79", Some("calls"), Some(64))
                .unwrap()
                .is_none()
        );
        let path = store
            .shortest_path_with_max_hops("node:00", "node:79", Some("calls"), Some(79))
            .unwrap()
            .unwrap();
        assert_eq!(path.hops, 79);
        assert_eq!(path.nodes.first().map(String::as_str), Some("node:00"));
        assert_eq!(path.nodes.last().map(String::as_str), Some("node:79"));

        let direct = store
            .shortest_path_with_max_hops("node:00", "node:79", Some("mentions"), Some(1))
            .unwrap()
            .unwrap();
        assert_eq!(direct.nodes, vec!["node:00", "node:79"]);
    }

    #[test]
    fn sqlite_resolves_evidence_targets_with_indexed_properties() {
        let store = SqliteGraphStore::in_memory().unwrap();
        for node in [
            GraphNode::new("gbak-refresh", "backlog", "#refresh")
                .with_property("ref_id", "refresh")
                .with_property("handle", "backlog-handle"),
            GraphNode::new("gjob-refresh", "job_packet", "do #refresh")
                .with_property("ref_id", "refresh"),
            GraphNode::new("gwres-refresh", "worker_result", "completed #refresh")
                .with_property("ref_id", "refresh"),
        ] {
            store.upsert_node(&node).unwrap();
        }

        let by_ref = store
            .resolve_evidence_target("#refresh", &["backlog", "job_packet", "worker_result"])
            .unwrap()
            .unwrap();
        assert_eq!(by_ref.id, "gbak-refresh");
        let by_handle = store
            .resolve_evidence_target("backlog-handle", &["backlog"])
            .unwrap()
            .unwrap();
        assert_eq!(by_handle.id, "gbak-refresh");
    }

    #[test]
    fn sqlite_schema_migration_backfills_materialized_node_properties() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            r#"
            PRAGMA user_version = 2;
            CREATE TABLE graph_nodes (
                id TEXT PRIMARY KEY,
                kind TEXT NOT NULL,
                label TEXT NOT NULL,
                properties_json TEXT NOT NULL DEFAULT '{}',
                provenance_json TEXT NOT NULL DEFAULT '[]',
                freshness_json TEXT,
                row_hash TEXT,
                source_watermark TEXT
            );
            CREATE INDEX idx_graph_nodes_kind ON graph_nodes(kind);
            CREATE TABLE graph_edges (
                from_id TEXT NOT NULL,
                to_id TEXT NOT NULL,
                kind TEXT NOT NULL,
                properties_json TEXT NOT NULL DEFAULT '{}',
                provenance_json TEXT NOT NULL DEFAULT '[]',
                freshness_json TEXT,
                row_hash TEXT,
                source_watermark TEXT,
                PRIMARY KEY (from_id, to_id, kind)
            );
            CREATE INDEX idx_graph_edges_from_kind ON graph_edges(from_id, kind);
            CREATE INDEX idx_graph_edges_to_kind ON graph_edges(to_id, kind);
            CREATE TABLE graph_projection_versions (
                scope TEXT PRIMARY KEY,
                projection_version TEXT NOT NULL,
                content_hash TEXT,
                source_watermark TEXT,
                observed_at_unix INTEGER NOT NULL
            );
            CREATE TABLE graph_tombstones (
                row_key TEXT PRIMARY KEY,
                row_kind TEXT NOT NULL,
                deleted_at_unix INTEGER NOT NULL
            );
            INSERT INTO graph_nodes
                (id, kind, label, properties_json, provenance_json)
            VALUES
                ('topic:rooms', 'topic', 'Rooms', '{"domain":"livekit"}', '[]'),
                ('topic:egress', 'topic', 'Egress', '{"domain":"recording"}', '[]');
            INSERT INTO graph_edges
                (from_id, to_id, kind, properties_json, provenance_json)
            VALUES
                ('topic:rooms', 'topic:egress', 'mentions', '{"confidence":"0.91"}', '[]');
            "#,
        )
        .unwrap();

        let store = SqliteGraphStore::from_connection(conn).unwrap();
        let version: i64 = store
            .conn
            .pragma_query_value(None, "user_version", |row| row.get(0))
            .unwrap();
        assert_eq!(version, SQLITE_GRAPH_SCHEMA_VERSION);
        let property_rows: usize = store
            .conn
            .query_row(
                "SELECT COUNT(*) FROM graph_node_properties WHERE key = 'domain'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(property_rows, 2);
        let edge_property_rows: usize = store
            .conn
            .query_row(
                "SELECT COUNT(*) FROM graph_edge_properties WHERE key = 'confidence'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(edge_property_rows, 1);
        let edge = store
            .edge(&GraphEdge::stable_id(
                "topic:rooms",
                "topic:egress",
                "mentions",
            ))
            .unwrap()
            .unwrap();
        assert_eq!(edge.properties.get("confidence"), Some(&"0.91".to_string()));

        let page = store
            .paged_nodes_by_kind(
                "topic",
                GraphQueryOptions {
                    property_filters: vec![GraphPropertyFilter {
                        key: "domain".to_string(),
                        value: "livekit".to_string(),
                    }],
                    ..GraphQueryOptions::default()
                },
            )
            .unwrap();
        assert_eq!(page.nodes[0].id, "topic:rooms");
        assert!(
            page.page
                .diagnostics
                .iter()
                .any(|diagnostic| diagnostic.contains("idx_graph_node_properties_key_value_node")),
            "{:?}",
            page.page.diagnostics
        );
    }

    #[test]
    fn sqlite_store_batches_reachable_nodes_by_kinds() {
        let store = SqliteGraphStore::in_memory().unwrap();
        for node in [
            GraphNode::new("start", "backlog", "start"),
            GraphNode::new("ctx", "worker_context", "context"),
            GraphNode::new("src", "source_handle", "source"),
            GraphNode::new("sem", "semantic_concept", "concept"),
        ] {
            store.upsert_node(&node).unwrap();
        }
        store
            .upsert_edge(&GraphEdge::new("start", "ctx", "has_context"))
            .unwrap();
        store
            .upsert_edge(&GraphEdge::new("ctx", "src", "uses_source"))
            .unwrap();
        store
            .upsert_edge(&GraphEdge::new("start", "sem", "mentions_concept"))
            .unwrap();

        let rows = store
            .reachable_nodes_by_kinds(
                "start",
                &["worker_context", "source_handle", "semantic_concept"],
                2,
                8,
            )
            .unwrap();
        assert_eq!(rows["worker_context"][0].0.id, "ctx");
        assert_eq!(
            rows["source_handle"][0].1.nodes,
            vec!["start", "ctx", "src"]
        );
        assert_eq!(rows["semantic_concept"][0].1.hops, 1);
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
        assert_eq!(refresh.deleted_nodes, 2);
        assert_eq!(refresh.deleted_edges, 3);
        assert_eq!(refresh.unchanged_nodes, 126);
        assert_eq!(refresh.unchanged_edges, 124);
        assert_eq!(refresh.upserted_nodes, 0);
        assert_eq!(refresh.upserted_edges, 0);
        assert_eq!(refresh.unchanged_properties, 250);
        assert_eq!(refresh.upserted_properties, 0);
        assert!(
            refresh
                .phase_timings
                .iter()
                .any(|phase| phase.name == "sqlite_node_staging"
                    && phase.detail.contains("bulk stage 126 graph_nodes rows")
                    && phase.detail.contains("multi-row chunks up to 50 rows")),
            "{:?}",
            refresh.phase_timings
        );
        assert!(
            refresh
                .phase_timings
                .iter()
                .any(|phase| phase.name == "sqlite_edge_staging"
                    && phase.detail.contains("bulk stage 124 graph_edges rows")
                    && phase.detail.contains("multi-row chunks up to 50 rows")),
            "{:?}",
            refresh.phase_timings
        );
        let staged_node_properties: usize = store
            .conn
            .query_row(
                "SELECT COUNT(*) FROM temp.next_graph_node_properties",
                [],
                |row| row.get(0),
            )
            .unwrap();
        let staged_edge_properties: usize = store
            .conn
            .query_row(
                "SELECT COUNT(*) FROM temp.next_graph_edge_properties",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(staged_node_properties, 0);
        assert_eq!(staged_edge_properties, 0);
        assert!(
            refresh
                .phase_timings
                .iter()
                .any(|phase| phase.name == "sqlite_property_row_staging"
                    && phase.detail.contains("new/changed node rows")),
            "{:?}",
            refresh.phase_timings
        );
        assert!(
            refresh
                .phase_timings
                .iter()
                .any(|phase| phase.name == "sqlite_edge_property_row_staging"
                    && phase.detail.contains("new/changed edge rows")),
            "{:?}",
            refresh.phase_timings
        );
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
