use anyhow::{Context, Result, bail};
use rusqlite::{
    Connection, OpenFlags, OptionalExtension, Row, params_from_iter,
    types::{Type, Value},
};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use std::cell::Cell;
use std::collections::{BTreeMap, BTreeSet};
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

pub use tsift_core::{
    ConvexEdgeRow, ConvexGraphClient, ConvexGraphStore, ConvexNodeRow, ConvexProjectionRows,
    ConvexRowsGraphClient, GRAPH_SEMANTIC_VECTOR_DEFAULT_MODEL,
    GRAPH_SEMANTIC_VECTOR_MODEL_PROPERTY_KEY, GRAPH_SEMANTIC_VECTOR_PROPERTY_KEY, GraphEdge,
    GraphFreshness, GraphNode, GraphPagedSubgraph, GraphPath, GraphProjection, GraphPropertyFilter,
    GraphProvenance, GraphQueryOptions, GraphQueryPage, GraphSemanticCandidate, GraphStore,
    GraphSubgraph, PropertyMode, RankedNeighborhoodOptions, RankedNeighborhoodResult,
    SQLITE_GRAPH_SCHEMA_VERSION, SemanticSeededNeighborhoodExpansion,
    SemanticSeededNeighborhoodOptions, TerseGraphEdge, TerseGraphNode, apply_graph_edge_query_page,
    apply_graph_query_page, graph_edge_id, graph_semantic_cosine,
    graph_semantic_top_candidates_by_property_scan, parse_graph_semantic_vector_property,
    shortest_path_using_outgoing, stable_graph_edge_id,
};

fn row_usize(row: &Row<'_>, idx: usize) -> rusqlite::Result<usize> {
    let value: i64 = row.get(idx)?;
    usize::try_from(value).map_err(|_| rusqlite::Error::IntegralValueOutOfRange(idx, value))
}

fn row_u64(row: &Row<'_>, idx: usize) -> rusqlite::Result<u64> {
    let value: i64 = row.get(idx)?;
    u64::try_from(value).map_err(|_| rusqlite::Error::IntegralValueOutOfRange(idx, value))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ReadOnlyRecovery {
    SnapshotFallback,
    SnapshotFallbackWal,
}

pub fn read_only_snapshot_recovery(
    db_path: &Path,
    err: &anyhow::Error,
) -> Option<ReadOnlyRecovery> {
    if !error_mentions_locked_db(err) {
        return None;
    }
    if wal_sidecar_path(db_path).exists() || shared_memory_sidecar_path(db_path).exists() {
        Some(ReadOnlyRecovery::SnapshotFallbackWal)
    } else if rollback_journal_path(db_path).exists() {
        Some(ReadOnlyRecovery::SnapshotFallback)
    } else {
        None
    }
}

pub fn rollback_journal_path(db_path: &Path) -> PathBuf {
    let mut journal = db_path.as_os_str().to_os_string();
    journal.push("-journal");
    PathBuf::from(journal)
}

pub fn wal_sidecar_path(db_path: &Path) -> PathBuf {
    let mut wal = db_path.as_os_str().to_os_string();
    wal.push("-wal");
    PathBuf::from(wal)
}

pub fn shared_memory_sidecar_path(db_path: &Path) -> PathBuf {
    let mut shm = db_path.as_os_str().to_os_string();
    shm.push("-shm");
    PathBuf::from(shm)
}

pub fn copy_read_only_snapshot(
    db_path: &Path,
    default_stem: &str,
) -> Result<(PathBuf, Vec<PathBuf>)> {
    let snapshot_path = snapshot_copy_path(db_path, default_stem);
    std::fs::copy(db_path, &snapshot_path).with_context(|| {
        format!(
            "copying locked db {} to snapshot {}",
            db_path.display(),
            snapshot_path.display()
        )
    })?;
    let mut cleanup_paths = vec![snapshot_path.clone()];
    copy_optional_snapshot_sidecar(
        &wal_sidecar_path(db_path),
        &wal_sidecar_path(&snapshot_path),
        &mut cleanup_paths,
    )?;
    copy_optional_snapshot_sidecar(
        &shared_memory_sidecar_path(db_path),
        &shared_memory_sidecar_path(&snapshot_path),
        &mut cleanup_paths,
    )?;
    Ok((snapshot_path, cleanup_paths))
}

pub fn error_mentions_locked_db(err: &anyhow::Error) -> bool {
    err.chain()
        .any(|cause| cause.to_string().contains("database is locked"))
}

fn copy_optional_snapshot_sidecar(
    source_path: &Path,
    snapshot_path: &Path,
    cleanup_paths: &mut Vec<PathBuf>,
) -> Result<()> {
    match std::fs::copy(source_path, snapshot_path) {
        Ok(_) => {
            cleanup_paths.push(snapshot_path.to_path_buf());
            Ok(())
        }
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(err) => Err(err).with_context(|| {
            format!(
                "copying SQLite sidecar {} to snapshot {}",
                source_path.display(),
                snapshot_path.display()
            )
        }),
    }
}

fn snapshot_copy_path(db_path: &Path, default_stem: &str) -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or(Duration::ZERO)
        .as_nanos();
    let stem = db_path
        .file_stem()
        .and_then(|stem| stem.to_str())
        .unwrap_or(default_stem);
    let mut file_name = OsString::from(format!("tsift-{stem}-{}-{nanos}", std::process::id()));
    file_name.push(".db");
    std::env::temp_dir().join(file_name)
}
const SQLITE_GRAPH_WAL_AUTOCHECKPOINT_PAGES: i64 = 4096;
const SQLITE_GRAPH_STAGING_CHUNK_ROWS: usize = 500;

/// SQLite-backed graph store.
///
/// # Temp-table invariant
///
/// Several methods — `replace_projection_with_version`, `upsert_projection`,
/// `edges_between_nodes`, and `breadth_first_search` — create connection-scoped
/// `TEMP TABLE` staging areas inside the underlying SQLite connection. Two
/// concurrent calls on the same `SqliteGraphStore` (or sharing the same
/// connection through `SqliteReadOnlyConnection`) would collide on those temp
/// table names and produce incorrect results or errors.
///
/// **Only one temp-table-using method may be active at a time per connection.**
pub struct SqliteGraphStore {
    conn: Connection,
    _snapshot_copy: Option<SnapshotCopyGuard>,
    read_only_recovery: Option<ReadOnlyRecovery>,
    temp_table_active: Cell<bool>,
}

/// Read-only handle to a SQLite graph database connection.
///
/// # Temp-table invariant
///
/// When this connection is unwrapped into a `SqliteGraphStore` via
/// `SqliteGraphStore::from_read_only_connection`, the same temp-table
/// concurrency rules apply: only one temp-table-using method
/// (`replace_projection_with_version`, `upsert_projection`,
/// `edges_between_nodes`, `breadth_first_search`) may be active at a time
/// per connection.
pub struct SqliteReadOnlyConnection {
    conn: Connection,
    _snapshot_copy: Option<SnapshotCopyGuard>,
    recovery: Option<ReadOnlyRecovery>,
}

static BFS_CALL_ID: AtomicU64 = AtomicU64::new(0);

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

fn sqlite_table_exists(conn: &Connection, table: &str) -> Result<bool> {
    conn.query_row(
        r#"
        SELECT EXISTS(
            SELECT 1
            FROM sqlite_master
            WHERE type = 'table' AND name = ?1
        )
        "#,
        [table],
        |row| row.get::<_, bool>(0),
    )
    .map_err(Into::into)
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
    if old_version < 6 {
        ensure_sqlite_graph_semantic_vectors_schema(conn)?;
        rebuild_graph_node_semantic_vectors(conn)?;
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

fn ensure_sqlite_graph_semantic_vectors_schema(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        r#"
        CREATE TABLE IF NOT EXISTS graph_node_semantic_vectors (
            node_id TEXT PRIMARY KEY,
            kind TEXT NOT NULL,
            model TEXT NOT NULL,
            dimensions INTEGER NOT NULL,
            vector_blob BLOB NOT NULL,
            FOREIGN KEY (node_id) REFERENCES graph_nodes(id) ON DELETE CASCADE
        );
        CREATE INDEX IF NOT EXISTS idx_graph_node_semantic_vectors_kind_dims
            ON graph_node_semantic_vectors(kind, dimensions, node_id);
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

struct GraphSemanticVectorRow {
    model: String,
    dimensions: usize,
    vector_blob: Vec<u8>,
}

fn graph_semantic_vector_row(
    properties: &BTreeMap<String, String>,
) -> Option<GraphSemanticVectorRow> {
    let vector = properties
        .get(GRAPH_SEMANTIC_VECTOR_PROPERTY_KEY)
        .and_then(|value| parse_graph_semantic_vector_property(value))?;
    Some(GraphSemanticVectorRow {
        model: properties
            .get(GRAPH_SEMANTIC_VECTOR_MODEL_PROPERTY_KEY)
            .cloned()
            .unwrap_or_else(|| GRAPH_SEMANTIC_VECTOR_DEFAULT_MODEL.to_string()),
        dimensions: vector.len(),
        vector_blob: semantic_vector_to_blob(&vector),
    })
}

fn semantic_vector_to_blob(vector: &[f64]) -> Vec<u8> {
    let mut blob = Vec::with_capacity(std::mem::size_of_val(vector));
    for value in vector {
        blob.extend_from_slice(&value.to_le_bytes());
    }
    blob
}

fn semantic_vector_from_blob(blob: &[u8], dimensions: usize) -> Option<Vec<f64>> {
    if dimensions == 0 || blob.len() != dimensions * std::mem::size_of::<f64>() {
        return None;
    }
    let mut vector = Vec::with_capacity(dimensions);
    for chunk in blob.chunks_exact(std::mem::size_of::<f64>()) {
        let value = f64::from_le_bytes(chunk.try_into().ok()?);
        if !value.is_finite() {
            return None;
        }
        vector.push(value);
    }
    Some(vector)
}

fn replace_node_semantic_vector(
    conn: &Connection,
    node_id: &str,
    kind: &str,
    properties: &BTreeMap<String, String>,
) -> Result<()> {
    conn.execute(
        "DELETE FROM graph_node_semantic_vectors WHERE node_id = ?1",
        [node_id],
    )?;
    let Some(row) = graph_semantic_vector_row(properties) else {
        return Ok(());
    };
    conn.execute(
        r#"
        INSERT INTO graph_node_semantic_vectors
            (node_id, kind, model, dimensions, vector_blob)
        VALUES (?1, ?2, ?3, ?4, ?5)
        "#,
        (
            node_id,
            kind,
            row.model,
            row.dimensions as i64,
            row.vector_blob,
        ),
    )?;
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

fn rebuild_graph_node_semantic_vectors(conn: &Connection) -> Result<()> {
    if !sqlite_column_exists(conn, "graph_nodes", "properties_json")?
        || !sqlite_table_exists(conn, "graph_node_semantic_vectors")?
    {
        return Ok(());
    }
    conn.execute("DELETE FROM graph_node_semantic_vectors", [])?;
    let rows = {
        let mut stmt = conn.prepare(
            r#"
            SELECT id, kind, properties_json
            FROM graph_nodes
            WHERE json_extract(properties_json, '$.embedding') IS NOT NULL
            ORDER BY id
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
    for (node_id, kind, properties_json) in rows {
        let properties: BTreeMap<String, String> = serde_json::from_str(&properties_json)
            .with_context(|| format!("parsing semantic properties for graph node {node_id}"))?;
        replace_node_semantic_vector(conn, &node_id, &kind, &properties)?;
    }
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

fn sqlite_stage_all_ids(
    tx: &rusqlite::Transaction<'_>,
    nodes: &[GraphNode],
    edges: &[GraphEdge],
) -> Result<()> {
    for chunk in nodes
        .iter()
        .map(|n| &n.id)
        .collect::<Vec<_>>()
        .chunks(SQLITE_GRAPH_STAGING_CHUNK_ROWS)
    {
        let placeholders: Vec<&str> = chunk.iter().map(|_| "(?)").collect();
        let sql = format!(
            "INSERT OR IGNORE INTO next_graph_all_node_ids (id) VALUES {}",
            placeholders.join(", ")
        );
        let values: Vec<Value> = chunk.iter().map(|id| Value::Text((*id).clone())).collect();
        tx.execute(&sql, params_from_iter(values.iter()))?;
    }
    for chunk in edges
        .iter()
        .map(graph_edge_id)
        .collect::<Vec<_>>()
        .chunks(SQLITE_GRAPH_STAGING_CHUNK_ROWS)
    {
        let placeholders: Vec<&str> = chunk.iter().map(|_| "(?)").collect();
        let sql = format!(
            "INSERT OR IGNORE INTO next_graph_all_edge_keys (edge_key) VALUES {}",
            placeholders.join(", ")
        );
        let values: Vec<Value> = chunk.iter().map(|ek| Value::Text(ek.to_string())).collect();
        tx.execute(&sql, params_from_iter(values.iter()))?;
    }
    Ok(())
}

fn sqlite_stage_projection_nodes(
    tx: &rusqlite::Transaction<'_>,
    nodes: &[&GraphNode],
    source_watermark: Option<&str>,
) -> Result<()> {
    let mut insert_semantic_vector = tx.prepare(
        r#"
        INSERT INTO next_graph_node_semantic_vectors
            (node_id, kind, model, dimensions, vector_blob)
        VALUES (?1, ?2, ?3, ?4, ?5)
        "#,
    )?;
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
        for node in chunk {
            let Some(row) = graph_semantic_vector_row(&node.properties) else {
                continue;
            };
            insert_semantic_vector.execute((
                &node.id,
                &node.kind,
                row.model,
                row.dimensions as i64,
                row.vector_blob,
            ))?;
        }
    }
    Ok(())
}

fn sqlite_stage_projection_edges(
    tx: &rusqlite::Transaction<'_>,
    edges: &[&GraphEdge],
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
    fn assert_not_in_temp_table_section(&self) {
        if self.temp_table_active.get() {
            panic!(
                "SqliteGraphStore: re-entrant temp-table call detected — only one temp-table-using method may be active at a time per connection"
            );
        }
    }

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

            CREATE TABLE IF NOT EXISTS graph_node_semantic_vectors (
                node_id TEXT PRIMARY KEY,
                kind TEXT NOT NULL,
                model TEXT NOT NULL,
                dimensions INTEGER NOT NULL,
                vector_blob BLOB NOT NULL,
                FOREIGN KEY (node_id) REFERENCES graph_nodes(id) ON DELETE CASCADE
            );
            CREATE INDEX IF NOT EXISTS idx_graph_node_semantic_vectors_kind_dims
                ON graph_node_semantic_vectors(kind, dimensions, node_id);

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
            temp_table_active: Cell::new(false),
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
            temp_table_active: Cell::new(false),
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
        self.assert_not_in_temp_table_section();
        self.temp_table_active.set(true);
        let scope = scope.into();
        let mut result = self.replace_projection_with_version_fallible(
            scope,
            projection,
            projection_version,
            source_watermark,
        );
        self.temp_table_active.set(false);
        if let Ok(ref mut refresh) = result {
            let total_rows = refresh.upserted_nodes + refresh.upserted_edges;
            let autocheckpoint = if total_rows > 10000 {
                8192
            } else if total_rows > 1000 {
                4096
            } else {
                2048
            };
            let _ = self
                .conn
                .pragma_update(None, "wal_autocheckpoint", autocheckpoint);
            // `PRAGMA wal_checkpoint(TRUNCATE)` reports its outcome in a result
            // row `(busy, log_frames, checkpointed_frames)` rather than erroring:
            // concurrent readers pin the WAL and yield `busy=1`, so the WAL is
            // not truncated. The old `let _ = execute_batch(...)` discarded that
            // row entirely, hiding unbounded `-wal` growth that later trips
            // snapshot-export/import sidecar checks. Record the outcome in
            // phase_timings instead of swallowing it (#gdbwalcheckpoint).
            let checkpoint_started = Instant::now();
            let checkpoint = self
                .conn
                .query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |row| {
                    Ok((
                        row.get::<_, i64>(0)?,
                        row.get::<_, i64>(1)?,
                        row.get::<_, i64>(2)?,
                    ))
                });
            let (name, detail) = match checkpoint {
                Ok((0, _log, _checkpointed)) => (
                    "wal_checkpoint:ok",
                    "wal_checkpoint(TRUNCATE) truncated the WAL".to_string(),
                ),
                Ok((_busy, log, checkpointed)) => (
                    "wal_checkpoint:busy",
                    format!(
                        "wal_checkpoint(TRUNCATE) was blocked by concurrent readers \
                         ({checkpointed}/{log} frames checkpointed, WAL not truncated); \
                         the -wal file may grow until readers release and a writer truncates it"
                    ),
                ),
                Err(err) => (
                    "wal_checkpoint:error",
                    format!(
                        "wal_checkpoint(TRUNCATE) failed: {err}; \
                         the -wal file may grow until a writer can checkpoint it"
                    ),
                ),
            };
            refresh.phase_timings.push(SqliteProjectionRefreshPhase {
                name: name.to_string(),
                duration_micros: checkpoint_started.elapsed().as_micros(),
                detail,
            });
        }
        result
    }

    fn replace_projection_with_version_fallible(
        &mut self,
        scope: String,
        projection: &GraphProjection,
        projection_version: Option<&str>,
        source_watermark: Option<String>,
    ) -> Result<SqliteProjectionRefresh> {
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
            CREATE TEMP TABLE IF NOT EXISTS next_graph_node_semantic_vectors (
                node_id TEXT PRIMARY KEY,
                kind TEXT NOT NULL,
                model TEXT NOT NULL,
                dimensions INTEGER NOT NULL,
                vector_blob BLOB NOT NULL
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
            CREATE TEMP TABLE IF NOT EXISTS next_graph_all_node_ids (
                id TEXT PRIMARY KEY
            );
            CREATE TEMP TABLE IF NOT EXISTS next_graph_all_edge_keys (
                edge_key TEXT PRIMARY KEY
            );
            DELETE FROM next_graph_nodes;
            DELETE FROM next_graph_edges;
            DELETE FROM next_graph_node_properties;
            DELETE FROM next_graph_node_semantic_vectors;
            DELETE FROM next_graph_edge_properties;
            DELETE FROM next_graph_changed_nodes;
            DELETE FROM next_graph_changed_edges;
            DELETE FROM next_graph_all_node_ids;
            DELETE FROM next_graph_all_edge_keys;
            "#,
        )?;
        phase_timings.push(sqlite_refresh_phase_timing(
            "sqlite_temp_table_prepare",
            started,
            "create and clear refresh staging tables before row loading",
        ));
        let (changed_nodes, changed_edges, skipped_nodes, skipped_edges) = if force_refresh_writes {
            (
                projection.nodes.iter().collect(),
                projection.edges.iter().collect(),
                0usize,
                0usize,
            )
        } else {
            let existing_node_hashes: BTreeMap<String, String> = {
                let mut stmt =
                    tx.prepare("SELECT id, row_hash FROM graph_nodes WHERE row_hash IS NOT NULL")?;
                let rows = stmt.query_map([], |row| {
                    Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
                })?;
                collect_rows(rows)?.into_iter().collect()
            };
            let existing_edge_hashes: BTreeMap<String, String> = {
                let mut stmt = tx.prepare(
                    "SELECT edge_key, row_hash FROM graph_edges WHERE row_hash IS NOT NULL",
                )?;
                let rows = stmt.query_map([], |row| {
                    Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
                })?;
                collect_rows(rows)?.into_iter().collect()
            };
            let cn: Vec<&GraphNode> = projection
                .nodes
                .iter()
                .filter(|n| {
                    let hash = row_hash(*n).ok();
                    hash.as_ref()
                        .is_none_or(|h| existing_node_hashes.get(&n.id) != Some(h))
                })
                .collect();
            let ce: Vec<&GraphEdge> = projection
                .edges
                .iter()
                .filter(|e| {
                    let hash = row_hash(*e).ok();
                    let ek = graph_edge_id(e);
                    hash.as_ref()
                        .is_none_or(|h| existing_edge_hashes.get(&ek) != Some(h))
                })
                .collect();
            let sn = projection.nodes.len() - cn.len();
            let se = projection.edges.len() - ce.len();
            (cn, ce, sn, se)
        };
        {
            let started = Instant::now();
            sqlite_stage_all_ids(&tx, &projection.nodes, &projection.edges)?;
            sqlite_stage_projection_nodes(&tx, &changed_nodes, source_watermark.as_deref())?;
            sqlite_stage_projection_edges(&tx, &changed_edges, source_watermark.as_deref())?;
            phase_timings.push(sqlite_refresh_phase_timing(
                "sqlite_node_staging",
                started,
                &format!(
                    "pre-filtered staging: {} nodes ({} unchanged skipped), {} edges ({} unchanged skipped) into temp tables using multi-row chunks up to {} rows",
                    changed_nodes.len(),
                    skipped_nodes,
                    changed_edges.len(),
                    skipped_edges,
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
            let sql = if force_refresh_writes {
                r#"
                SELECT g.id
                FROM graph_nodes g
                LEFT JOIN next_graph_nodes n ON n.id = g.id
                WHERE n.id IS NULL
                ORDER BY g.id
                "#
            } else {
                r#"
                SELECT g.id
                FROM graph_nodes g
                LEFT JOIN next_graph_all_node_ids n ON n.id = g.id
                WHERE n.id IS NULL
                ORDER BY g.id
                "#
            };
            let mut stmt = tx.prepare(sql)?;
            collect_rows(stmt.query_map([], |row| row.get::<_, String>(0))?)?
        };
        let tombstoned_edges = {
            let sql = if force_refresh_writes {
                r#"
                SELECT g.edge_key
                FROM graph_edges g
                LEFT JOIN next_graph_edges n
                    ON n.edge_key = g.edge_key
                WHERE n.edge_key IS NULL
                ORDER BY g.edge_key
                "#
            } else {
                r#"
                SELECT g.edge_key
                FROM graph_edges g
                LEFT JOIN next_graph_all_edge_keys n
                    ON n.edge_key = g.edge_key
                WHERE n.edge_key IS NULL
                ORDER BY g.edge_key
                "#
            };
            let mut stmt = tx.prepare(sql)?;
            collect_rows(stmt.query_map([], |row| row.get::<_, String>(0))?)?
        };
        let unchanged_nodes: usize = if force_refresh_writes {
            tx.query_row(
                r#"
                SELECT COUNT(*)
                FROM next_graph_nodes n
                JOIN graph_nodes g ON g.id = n.id
                WHERE g.row_hash = n.row_hash
                "#,
                [],
                |row| row_usize(row, 0),
            )?
        } else {
            skipped_nodes
        };
        let unchanged_edges: usize = if force_refresh_writes {
            tx.query_row(
                r#"
                SELECT COUNT(*)
                FROM next_graph_edges n
                JOIN graph_edges g
                    ON g.edge_key = n.edge_key
                WHERE g.row_hash = n.row_hash
                "#,
                [],
                |row| row_usize(row, 0),
            )?
        } else {
            skipped_edges
        };
        let reused_owner_node_properties: usize = if force_refresh_writes {
            tx.query_row(
                r#"
                SELECT COUNT(*)
                FROM graph_node_properties g
                JOIN next_graph_nodes n ON n.id = g.node_id
                LEFT JOIN next_graph_changed_nodes c ON c.id = n.id
                WHERE c.id IS NULL
                "#,
                [],
                |row| row_usize(row, 0),
            )?
        } else {
            tx.query_row(
                r#"
                SELECT COUNT(*)
                FROM graph_node_properties g
                JOIN next_graph_all_node_ids a ON a.id = g.node_id
                LEFT JOIN next_graph_changed_nodes c ON c.id = a.id
                WHERE c.id IS NULL
                "#,
                [],
                |row| row_usize(row, 0),
            )?
        };
        let reused_owner_edge_properties: usize = if force_refresh_writes {
            tx.query_row(
                r#"
                SELECT COUNT(*)
                FROM graph_edge_properties g
                JOIN next_graph_edges n ON n.edge_key = g.edge_key
                LEFT JOIN next_graph_changed_edges c ON c.edge_key = n.edge_key
                WHERE c.edge_key IS NULL
                "#,
                [],
                |row| row_usize(row, 0),
            )?
        } else {
            tx.query_row(
                r#"
                SELECT COUNT(*)
                FROM graph_edge_properties g
                JOIN next_graph_all_edge_keys a ON a.edge_key = g.edge_key
                LEFT JOIN next_graph_changed_edges c ON c.edge_key = a.edge_key
                WHERE c.edge_key IS NULL
                "#,
                [],
                |row| row_usize(row, 0),
            )?
        };
        let unchanged_changed_node_properties: usize = tx.query_row(
            r#"
            SELECT COUNT(*)
            FROM next_graph_node_properties n
            JOIN graph_node_properties g
                ON g.node_id = n.node_id AND g.key = n.key
            WHERE g.value = n.value
            "#,
            [],
            |row| row_usize(row, 0),
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
            |row| row_usize(row, 0),
        )?;
        let unchanged_properties = reused_owner_node_properties
            + reused_owner_edge_properties
            + unchanged_changed_node_properties
            + unchanged_changed_edge_properties;

        let deleted_edges = if force_refresh_writes {
            tx.execute(
                r#"
                DELETE FROM graph_edges
                WHERE NOT EXISTS (
                    SELECT 1
                    FROM next_graph_edges n
                    WHERE n.edge_key = graph_edges.edge_key
                )
                "#,
                [],
            )?
        } else {
            tx.execute(
                r#"
                DELETE FROM graph_edges
                WHERE NOT EXISTS (
                    SELECT 1
                    FROM next_graph_all_edge_keys n
                    WHERE n.edge_key = graph_edges.edge_key
                )
                "#,
                [],
            )?
        };
        let deleted_nodes = if force_refresh_writes {
            tx.execute(
                r#"
                DELETE FROM graph_nodes
                WHERE NOT EXISTS (
                    SELECT 1
                    FROM next_graph_nodes n
                    WHERE n.id = graph_nodes.id
                )
                "#,
                [],
            )?
        } else {
            tx.execute(
                r#"
                DELETE FROM graph_nodes
                WHERE NOT EXISTS (
                    SELECT 1
                    FROM next_graph_all_node_ids n
                    WHERE n.id = graph_nodes.id
                )
                "#,
                [],
            )?
        };

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
        tx.execute(
            r#"
                DELETE FROM graph_node_semantic_vectors
                WHERE EXISTS (
                    SELECT 1
                    FROM next_graph_changed_nodes c
                    WHERE c.id = graph_node_semantic_vectors.node_id
                )
                AND NOT EXISTS (
                    SELECT 1
                    FROM next_graph_node_semantic_vectors n
                    WHERE n.node_id = graph_node_semantic_vectors.node_id
                )
                "#,
            [],
        )?;
        tx.execute(
            r#"
                INSERT INTO graph_node_semantic_vectors
                    (node_id, kind, model, dimensions, vector_blob)
                SELECT n.node_id, n.kind, n.model, n.dimensions, n.vector_blob
                FROM next_graph_node_semantic_vectors n
                WHERE true
                ON CONFLICT(node_id) DO UPDATE SET
                    kind = excluded.kind,
                    model = excluded.model,
                    dimensions = excluded.dimensions,
                    vector_blob = excluded.vector_blob
                "#,
            [],
        )?;
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
            |row| row_usize(row, 0),
        )?;
        let tombstone_edge_count: usize = tx.query_row(
            "SELECT COUNT(*) FROM graph_tombstones WHERE row_kind = 'edge'",
            [],
            |row| row_usize(row, 0),
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

    /// Derive a Semantic Ontology Graph (`#memgraphrag-ont`, MemGraphRAG arxiv
    /// 2606.00610 third layer) from the instance graph: one `ontology_type` node
    /// per distinct node kind, and one `ontology_relation:<edge_kind>` edge per
    /// observed `(from_kind, edge_kind, to_kind)` triple, each carrying an
    /// `instance_count`. This data-driven schema lets retrieval start from
    /// abstract types and prune by permitted inter-type relations. Existing
    /// ontology rows are excluded so the derivation is idempotent and never folds
    /// the ontology layer into itself.
    pub fn derive_ontology(&self) -> Result<GraphProjection> {
        let mut projection = GraphProjection::default();

        let mut node_stmt = self.conn.prepare(
            "SELECT kind, COUNT(*) FROM graph_nodes \
             WHERE kind != 'ontology_type' \
             GROUP BY kind ORDER BY kind",
        )?;
        let node_rows = node_stmt.query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?))
        })?;
        for row in node_rows {
            let (kind, count) = row?;
            projection.nodes.push(
                GraphNode::new(format!("ontology_type:{kind}"), "ontology_type", &kind)
                    .with_property("type_kind", &kind)
                    .with_property("instance_count", count.to_string())
                    .with_provenance(GraphProvenance::new("tsift-ontology", &kind)),
            );
        }

        let mut rel_stmt = self.conn.prepare(
            "SELECT n1.kind, e.kind, n2.kind, COUNT(*) \
             FROM graph_edges e \
             JOIN graph_nodes n1 ON e.from_id = n1.id \
             JOIN graph_nodes n2 ON e.to_id = n2.id \
             WHERE e.kind NOT LIKE 'ontology_relation:%' \
               AND n1.kind != 'ontology_type' AND n2.kind != 'ontology_type' \
             GROUP BY n1.kind, e.kind, n2.kind \
             ORDER BY n1.kind, e.kind, n2.kind",
        )?;
        let rel_rows = rel_stmt.query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, i64>(3)?,
            ))
        })?;
        for row in rel_rows {
            let (from_kind, edge_kind, to_kind, count) = row?;
            projection.edges.push(
                GraphEdge::new(
                    format!("ontology_type:{from_kind}"),
                    format!("ontology_type:{to_kind}"),
                    format!("ontology_relation:{edge_kind}"),
                )
                .with_property("edge_kind", &edge_kind)
                .with_property("instance_count", count.to_string())
                .with_provenance(GraphProvenance::new("tsift-ontology", &edge_kind)),
            );
        }

        Ok(projection)
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
            WHERE graph_nodes.row_hash IS NOT excluded.row_hash
               OR graph_nodes.source_watermark IS NOT excluded.source_watermark
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
                let changed = insert_node.execute((
                    &node.id,
                    &node.kind,
                    &node.label,
                    to_json(&node.properties)?,
                    to_json(&node.provenance)?,
                    optional_to_json(&node.freshness)?,
                    row_hash(node)?,
                ))?;
                if changed > 0 {
                    delete_properties.execute([&node.id])?;
                    for (key, value) in &node.properties {
                        insert_property.execute((&node.id, key, value))?;
                    }
                    replace_node_semantic_vector(&tx, &node.id, &node.kind, &node.properties)?;
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
            WHERE graph_edges.row_hash IS NOT excluded.row_hash
               OR graph_edges.source_watermark IS NOT excluded.source_watermark
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
                let changed = insert_edge.execute((
                    &edge_key,
                    &edge.from_id,
                    &edge.to_id,
                    &edge.kind,
                    to_json(&edge.properties)?,
                    to_json(&edge.provenance)?,
                    optional_to_json(&edge.freshness)?,
                    row_hash(edge)?,
                ))?;
                if changed > 0 {
                    delete_properties.execute([&edge_key])?;
                    for (key, value) in &edge.properties {
                        insert_property.execute((&edge_key, key, value))?;
                    }
                }
            }
        }
        tx.commit()?;
        Ok(())
    }

    /// #kgrefreshdup: delete every node previously projected from `source_ref` by
    /// `provider`, so a re-extraction REPLACES that source's subgraph instead of
    /// accumulating duplicate canonical entities across refresh cycles. Incident
    /// edges, node/edge properties, and semantic vectors are removed automatically
    /// via the `ON DELETE CASCADE` foreign keys. The `provider` scope keeps the
    /// delete from touching non-KG nodes (e.g. AST symbols) that may share a
    /// `source_ref` path in the same graph database. Returns the node count deleted.
    pub fn delete_source_projection(&mut self, source_ref: &str, provider: &str) -> Result<usize> {
        let deleted = self.conn.execute(
            r#"
            DELETE FROM graph_nodes
            WHERE id IN (
                SELECT node_id FROM graph_node_properties
                WHERE key = 'source_ref' AND value = ?1
            )
            AND id IN (
                SELECT node_id FROM graph_node_properties
                WHERE key = 'provider' AND value = ?2
            )
            "#,
            (source_ref, provider),
        )?;
        Ok(deleted)
    }

    /// #kgsameas: link nodes of `kind` that share the same `id_key` property value
    /// (restricted to values starting with `id_prefix`) using star `edge_kind`
    /// edges — each group's members point at its lexicographically-smallest node
    /// id. This collapses duplicate logical entities for ALL graph consumers (not
    /// just the context pack), without deleting any node, so per-source provenance
    /// is preserved. Idempotent: edges key on (from,to,kind), so re-running upserts
    /// the same set. Returns the number of link edges written.
    pub fn link_nodes_by_shared_property(
        &mut self,
        kind: &str,
        id_key: &str,
        id_prefix: &str,
        edge_kind: &str,
        edge_properties: &[(&str, &str)],
    ) -> Result<usize> {
        // Read members grouped by shared id value (ordered so the representative
        // is deterministic: the smallest node id within each group).
        let mut groups: BTreeMap<String, Vec<String>> = BTreeMap::new();
        {
            let mut stmt = self.conn.prepare(
                "SELECT p.value, n.id \
                 FROM graph_nodes n \
                 JOIN graph_node_properties p ON p.node_id = n.id \
                 WHERE n.kind = ?1 AND p.key = ?2 AND p.value LIKE ?3 || '%' \
                 ORDER BY p.value, n.id",
            )?;
            let rows = stmt.query_map((kind, id_key, id_prefix), |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })?;
            for row in rows {
                let (shared, node_id) = row?;
                groups.entry(shared).or_default().push(node_id);
            }
        }

        let mut edges: Vec<GraphEdge> = Vec::new();
        for members in groups.values() {
            let Some((rep, rest)) = members.split_first() else {
                continue;
            };
            for member in rest {
                let mut edge = GraphEdge::new(member.clone(), rep.clone(), edge_kind);
                for (key, value) in edge_properties {
                    edge = edge.with_property(*key, *value);
                }
                edges.push(edge);
            }
        }

        let count = edges.len();
        if count > 0 {
            self.upsert_projection(&GraphProjection {
                nodes: Vec::new(),
                edges,
            })?;
        }
        Ok(count)
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

    fn edges_between_nodes_inline(&self, node_ids: &BTreeSet<String>) -> Result<Vec<GraphEdge>> {
        let placeholders: Vec<&str> = node_ids.iter().map(|_| "?").collect();
        let in_clause = placeholders.join(", ");
        let sql = format!(
            "SELECT e.edge_key, e.from_id, e.to_id, e.kind, e.properties_json, e.provenance_json, e.freshness_json \
             FROM graph_edges e \
             WHERE e.from_id IN ({in_clause}) \
               AND e.to_id IN ({in_clause}) \
             ORDER BY e.from_id, e.kind, e.to_id"
        );
        let values: Vec<Value> = node_ids
            .iter()
            .chain(node_ids.iter())
            .map(|id| Value::Text(id.clone()))
            .collect();
        let mut stmt = self.conn.prepare(&sql)?;
        collect_rows(stmt.query_map(params_from_iter(values.iter()), edge_from_row)?)
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

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TerseDiagnostic {
    pub code: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub index: Option<String>,
}

#[allow(dead_code)]
fn terse_query_plan_diagnostics(
    plan: &[String],
    expected_indexes: &[&str],
) -> Vec<TerseDiagnostic> {
    let mut diagnostics = vec![TerseDiagnostic {
        code: "plan_active",
        index: None,
    }];
    for expected_index in expected_indexes {
        if plan.iter().any(|row| row.contains(expected_index)) {
            diagnostics.push(TerseDiagnostic {
                code: "idx_ok",
                index: Some(expected_index.to_string()),
            });
        } else {
            diagnostics.push(TerseDiagnostic {
                code: "idx_missing",
                index: Some(expected_index.to_string()),
            });
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

fn sqlite_semantic_seeded_edge_score_expr(edge_alias: &str, direction_bonus: &str) -> String {
    format!(
        "(CASE {edge_alias}.kind \
WHEN 'semantic_relation' THEN 340 \
WHEN 'mentions_entity' THEN 280 \
WHEN 'mentions_concept' THEN 280 \
WHEN 'tagged_entity' THEN 280 \
WHEN 'tagged_concept' THEN 280 \
WHEN 'related_concept' THEN 280 \
WHEN 'mentions' THEN 220 \
WHEN 'calls' THEN 200 \
WHEN 'requests_context' THEN 180 \
WHEN 'scopes_context' THEN 180 \
WHEN 'scopes_source' THEN 180 \
WHEN 'explains_result' THEN 180 \
WHEN 'defines' THEN 120 \
WHEN 'contains' THEN 120 \
WHEN 'belongs_to' THEN 120 \
WHEN {edge_alias}.kind LIKE '%community%' THEN 200 \
WHEN {edge_alias}.kind LIKE '%semantic%' \
OR {edge_alias}.kind LIKE '%concept%' \
OR {edge_alias}.kind LIKE '%entity%' THEN 240 \
ELSE 80 END) \
+ ({direction_bonus}) \
+ (CASE {edge_alias}.kind \
WHEN 'mentions_concept' THEN 30 \
WHEN 'mentions_entity' THEN 30 \
WHEN 'tagged_concept' THEN 30 \
WHEN 'tagged_entity' THEN 30 \
WHEN 'related_concept' THEN 30 \
WHEN 'semantic_relation' THEN 28 \
WHEN 'calls' THEN 24 \
WHEN 'mentions' THEN 22 \
WHEN 'requests_context' THEN 18 \
WHEN 'scopes_context' THEN 18 \
WHEN 'scopes_source' THEN 18 \
WHEN 'explains_result' THEN 18 \
WHEN 'defines' THEN 12 \
WHEN 'contains' THEN 12 \
WHEN 'belongs_to' THEN 12 \
ELSE 0 END)"
    )
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
        replace_node_semantic_vector(&self.conn, &node.id, &node.kind, &node.properties)?;
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

    fn nodes_by_ids(&self, ids: &[String]) -> Result<Vec<GraphNode>> {
        let unique_ids = ids.iter().cloned().collect::<BTreeSet<_>>();
        if unique_ids.is_empty() {
            return Ok(Vec::new());
        }

        let mut nodes = Vec::new();
        let id_refs = unique_ids.iter().collect::<Vec<_>>();
        for chunk in id_refs.chunks(450) {
            let placeholders = chunk.iter().map(|_| "?").collect::<Vec<_>>().join(", ");
            let sql = format!(
                "SELECT id, kind, label, properties_json, provenance_json, freshness_json \
FROM graph_nodes \
WHERE id IN ({placeholders}) \
ORDER BY id"
            );
            let values = chunk
                .iter()
                .map(|id| Value::Text((*id).clone()))
                .collect::<Vec<_>>();
            let mut stmt = self.conn.prepare(&sql)?;
            nodes.extend(collect_rows(
                stmt.query_map(params_from_iter(values.iter()), node_from_row)?,
            )?);
        }
        nodes.sort_by(|left, right| left.id.cmp(&right.id));
        Ok(nodes)
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
                row_usize(row, 0)
            })?;
        let edges = self
            .conn
            .query_row("SELECT COUNT(*) FROM graph_edges", [], |row| {
                row_usize(row, 0)
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

    fn semantic_top_candidates(
        &self,
        query_vector: &[f64],
        kinds: &[&str],
        limit: usize,
    ) -> Result<Vec<GraphSemanticCandidate>> {
        if query_vector.is_empty() || kinds.is_empty() {
            return Ok(Vec::new());
        }
        if !sqlite_table_exists(&self.conn, "graph_node_semantic_vectors")? {
            return graph_semantic_top_candidates_by_property_scan(
                self,
                query_vector,
                kinds,
                limit,
            );
        }

        let unique_kinds = kinds.iter().copied().collect::<BTreeSet<_>>();
        if unique_kinds.is_empty() {
            return Ok(Vec::new());
        }
        let kind_placeholders = unique_kinds
            .iter()
            .map(|_| "?")
            .collect::<Vec<_>>()
            .join(", ");
        let sql = format!(
            r#"
            SELECT n.id, n.kind, n.label, n.properties_json, n.provenance_json, n.freshness_json,
                   graph_node_semantic_vectors.vector_blob,
                   graph_node_semantic_vectors.dimensions
            FROM graph_node_semantic_vectors INDEXED BY idx_graph_node_semantic_vectors_kind_dims
            JOIN graph_nodes n ON n.id = graph_node_semantic_vectors.node_id
            WHERE graph_node_semantic_vectors.dimensions = ?
              AND graph_node_semantic_vectors.kind IN ({kind_placeholders})
            ORDER BY graph_node_semantic_vectors.kind, n.label, n.id
            "#
        );
        let mut values = vec![Value::Integer(query_vector.len() as i64)];
        values.extend(
            unique_kinds
                .into_iter()
                .map(|kind| Value::Text(kind.to_string())),
        );
        let rows = {
            let mut stmt = self.conn.prepare(&sql)?;
            collect_rows(stmt.query_map(params_from_iter(values.iter()), |row| {
                Ok((
                    node_from_row_at(row, 0)?,
                    row.get::<_, Vec<u8>>(6)?,
                    row.get::<_, i64>(7)?,
                ))
            })?)?
        };

        let mut candidates = rows
            .into_iter()
            .filter_map(|(node, blob, dimensions)| {
                let dimensions = usize::try_from(dimensions).ok()?;
                let vector = semantic_vector_from_blob(&blob, dimensions)?;
                Some(GraphSemanticCandidate {
                    score: graph_semantic_cosine(query_vector, &vector),
                    node,
                })
            })
            .collect::<Vec<_>>();
        candidates.sort_by(|left, right| {
            right
                .score
                .partial_cmp(&left.score)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| left.node.kind.cmp(&right.node.kind))
                .then_with(|| left.node.label.cmp(&right.node.label))
                .then_with(|| left.node.id.cmp(&right.node.id))
        });
        if limit > 0 && candidates.len() > limit {
            candidates.truncate(limit);
        }
        Ok(candidates)
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

    fn semantic_seeded_expansion_edges(
        &self,
        current_id: &str,
        options: &SemanticSeededNeighborhoodOptions,
    ) -> Result<SemanticSeededNeighborhoodExpansion> {
        let from_score = sqlite_semantic_seeded_edge_score_expr("e", "8");
        let to_score = sqlite_semantic_seeded_edge_score_expr(
            "e",
            "CASE WHEN e.from_id = e.to_id THEN 8 ELSE 4 END",
        );
        let limit_clause = if options.edge_scan_cap > 0 {
            "LIMIT ?"
        } else {
            ""
        };
        let sql = format!(
            r#"
WITH candidate_edges AS (
    SELECT e.edge_key, e.from_id, e.to_id, e.kind, e.properties_json, e.provenance_json, e.freshness_json,
           {from_score} AS score
    FROM graph_edges e INDEXED BY idx_graph_edges_from_kind
    WHERE e.from_id = ?
    UNION ALL
    SELECT e.edge_key, e.from_id, e.to_id, e.kind, e.properties_json, e.provenance_json, e.freshness_json,
           {to_score} AS score
    FROM graph_edges e INDEXED BY idx_graph_edges_to_kind
    WHERE e.to_id = ?
),
ranked_edges AS (
    SELECT edge_key, from_id, to_id, kind, properties_json, provenance_json, freshness_json,
           MAX(score) AS score
    FROM candidate_edges
    GROUP BY edge_key, from_id, to_id, kind, properties_json, provenance_json, freshness_json
),
limited_edges AS (
    SELECT edge_key, from_id, to_id, kind, properties_json, provenance_json, freshness_json,
           score, COUNT(*) OVER () AS total_edges
    FROM ranked_edges
    ORDER BY score DESC, edge_key ASC
    {limit_clause}
)
SELECT edge_key, from_id, to_id, kind, properties_json, provenance_json, freshness_json, total_edges
FROM limited_edges
ORDER BY score DESC, edge_key ASC
"#
        );
        let mut values = vec![
            Value::Text(current_id.to_string()),
            Value::Text(current_id.to_string()),
        ];
        if options.edge_scan_cap > 0 {
            values.push(Value::Integer(
                options
                    .edge_scan_cap
                    .saturating_add(1)
                    .min(i64::MAX as usize) as i64,
            ));
        }
        let mut stmt = self.conn.prepare(&sql)?;
        let rows = collect_rows(stmt.query_map(params_from_iter(values.iter()), |row| {
            Ok((edge_from_row_at(row, 0)?, row.get::<_, i64>(7)? as usize))
        })?)?;
        let total_candidates = rows.first().map(|(_, total)| *total).unwrap_or(0);
        let mut edges = rows.into_iter().map(|(edge, _)| edge).collect::<Vec<_>>();
        let mut skipped_by_edge_cap = 0usize;
        if options.edge_scan_cap > 0 && total_candidates > options.edge_scan_cap {
            skipped_by_edge_cap = total_candidates - options.edge_scan_cap;
            edges.truncate(options.edge_scan_cap);
        }
        Ok(SemanticSeededNeighborhoodExpansion {
            edges,
            skipped_by_edge_cap,
        })
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
                |row| row_usize(row, 0),
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
        if node_ids.len() <= 20 {
            return self.edges_between_nodes_inline(node_ids);
        }
        self.assert_not_in_temp_table_section();
        self.temp_table_active.set(true);
        let result = (|| -> Result<Vec<GraphEdge>> {
            let tx = self.conn.unchecked_transaction()?;
            tx.execute_batch(
                r#"
                CREATE TEMP TABLE IF NOT EXISTS _edges_between_ids (id TEXT PRIMARY KEY);
                DELETE FROM _edges_between_ids;
                "#,
            )?;
            for chunk in node_ids.iter().collect::<Vec<_>>().chunks(450) {
                let row_placeholders: Vec<String> =
                    chunk.iter().map(|_| "(?)".to_string()).collect();
                let placeholders = row_placeholders.join(", ");
                let sql =
                    format!("INSERT OR IGNORE INTO _edges_between_ids (id) VALUES {placeholders}");
                let values: Vec<Value> =
                    chunk.iter().map(|id| Value::Text((*id).clone())).collect();
                tx.execute(&sql, params_from_iter(values.iter()))?;
            }
            let edges = {
                let mut stmt = tx.prepare(
                    r#"
                    SELECT e.edge_key, e.from_id, e.to_id, e.kind, e.properties_json, e.provenance_json, e.freshness_json
                    FROM graph_edges e
                    WHERE EXISTS (SELECT 1 FROM _edges_between_ids f WHERE f.id = e.from_id)
                      AND EXISTS (SELECT 1 FROM _edges_between_ids t WHERE t.id = e.to_id)
                    ORDER BY e.from_id, e.kind, e.to_id
                    "#,
                )?;
                collect_rows(stmt.query_map([], edge_from_row)?)?
            };
            tx.finish()?;
            Ok(edges)
        })();
        self.temp_table_active.set(false);
        result
    }

    fn ranked_neighborhood(
        &self,
        center_id: &str,
        options: &RankedNeighborhoodOptions,
    ) -> Result<Option<RankedNeighborhoodResult>> {
        if self.node(center_id)?.is_none() {
            return Ok(None);
        }
        let center = self.node(center_id)?.unwrap();

        let base_score_expr = match options.scoring {
            tsift_core::NeighborhoodScoring::BreadthFirst => {
                "MAX(0, 120 - (walk.depth * 18))".to_string()
            }
            tsift_core::NeighborhoodScoring::EdgeKindWeighted => {
                "MAX(0, 120 - (walk.depth * 18)) + CASE walk.edge_kind \
                 WHEN 'semantic_relation' THEN 34 \
                 WHEN 'mentions_entity' THEN 28 \
                 WHEN 'mentions_concept' THEN 28 \
                 WHEN 'tagged_entity' THEN 28 \
                 WHEN 'tagged_concept' THEN 28 \
                 WHEN 'related_concept' THEN 28 \
                 WHEN 'mentions' THEN 22 \
                 WHEN 'calls' THEN 20 \
                 WHEN 'requests_context' THEN 18 \
                 WHEN 'scopes_context' THEN 18 \
                 WHEN 'scopes_source' THEN 18 \
                 WHEN 'explains_result' THEN 18 \
                 WHEN 'defines' THEN 12 \
                 WHEN 'contains' THEN 12 \
                 WHEN 'belongs_to' THEN 12 \
                 ELSE 8 END".to_string()
            }
            tsift_core::NeighborhoodScoring::DegreeWeighted => {
                "MAX(0, 120 - (walk.depth * 18)) + CASE \
                 WHEN COALESCE((SELECT degree FROM degree_cache dc WHERE dc.id = walk.id), 0) <= 3 THEN 20 \
                 WHEN COALESCE((SELECT degree FROM degree_cache dc WHERE dc.id = walk.id), 0) <= 10 THEN 10 \
                 ELSE 0 END"
                    .to_string()
            }
        };
        let now_unix = options.observed_at_now_unix.unwrap_or_else(unix_now);
        let observed_at_half_life_secs = options.observed_at_half_life_secs.max(1);
        let observed_at_weight = options.observed_at_weight.max(0);
        let memory_node_boost = options.memory_node_boost.max(0);
        let observed_at_value = "COALESCE(\
            CAST(json_extract(n_score.freshness_json, '$.observed_at_unix') AS INTEGER), \
            CAST(json_extract(n_score.properties_json, '$.observed_at_unix') AS INTEGER), \
            CAST(json_extract(n_score.properties_json, '$.max_observed_at_unix') AS INTEGER)\
        )";
        let observed_at_expr = if observed_at_weight == 0 {
            "0".to_string()
        } else {
            format!(
                "CASE \
                 WHEN {observed_at_value} IS NULL THEN 0 \
                 WHEN ({now_unix} - {observed_at_value}) < {observed_at_half_life_secs} THEN {observed_at_weight} \
                 WHEN ({now_unix} - {observed_at_value}) < ({observed_at_half_life_secs} * 2) THEN {observed_at_weight} / 2 \
                 WHEN ({now_unix} - {observed_at_value}) < ({observed_at_half_life_secs} * 4) THEN {observed_at_weight} / 4 \
                 ELSE 0 END"
            )
        };
        let confidence_value =
            "CAST(json_extract(n_score.properties_json, '$.confidence') AS REAL)";
        let memory_signal_expr = if memory_node_boost == 0 {
            "0".to_string()
        } else {
            format!(
                "(CASE \
                  WHEN n_score.kind = 'memory_projection' THEN 0 \
                  WHEN n_score.kind IN ('finding', 'decision', 'memory_event') THEN {memory_node_boost} \
                  WHEN n_score.kind IN ('note', 'memory_session') THEN {memory_node_boost} / 2 \
                  WHEN n_score.kind IN ('source_handle', 'semantic_concept', 'semantic_vector_handle') \
                    AND json_extract(n_score.properties_json, '$.provider') = 'tsift-memory' THEN {memory_node_boost} / 2 \
                  WHEN n_score.kind LIKE 'memory_%' THEN {memory_node_boost} \
                  WHEN json_extract(n_score.properties_json, '$.provider') = 'tsift-memory' THEN {memory_node_boost} / 2 \
                  ELSE 0 END) \
                 + (CASE \
                  WHEN json_extract(n_score.properties_json, '$.confidence') IS NULL THEN 0 \
                  WHEN {confidence_value} <= 0.0 THEN 0 \
                  WHEN {confidence_value} >= 1.0 THEN {memory_node_boost} \
                  ELSE CAST(ROUND({memory_node_boost} * {confidence_value}) AS INTEGER) END)"
            )
        };
        let score_expr =
            format!("({base_score_expr}) + ({observed_at_expr}) + ({memory_signal_expr})");

        let use_degree_cache = matches!(
            options.scoring,
            tsift_core::NeighborhoodScoring::DegreeWeighted
        );
        let degree_cte = if use_degree_cache {
            "degree_cache AS ( \
            SELECT id, (SELECT COUNT(*) FROM graph_edges e WHERE e.from_id = n.id OR e.to_id = n.id) AS degree \
            FROM graph_nodes n), "
        } else {
            ""
        };
        let mut sql = format!(
            r#"
WITH RECURSIVE {degree_cte}walk(id, depth, edge_kind, score) AS (
SELECT ?, 0, '', ?
UNION
SELECT e.to_id, walk.depth + 1, e.kind,
"#,
        );
        sql.push_str(&format!("    {}\n", score_expr));
        sql.push_str(
            r#"
FROM walk
JOIN graph_edges e INDEXED BY idx_graph_edges_from_kind
ON e.from_id = walk.id
JOIN graph_nodes n_score ON n_score.id = e.to_id
WHERE walk.depth < ?
"#,
        );
        let mut values = vec![
            Value::Text(center_id.to_string()),
            Value::Integer(i64::MAX),
            Value::Integer(options.depth as i64),
        ];
        if let Some(kind) = &options.edge_kind {
            sql.push_str(" AND e.kind = ?");
            values.push(Value::Text(kind.clone()));
        }
        sql.push_str(
            r#"
            ),
scored_nodes AS (
SELECT walk.id, MAX(walk.score) AS score,
n.kind AS node_kind, n.label, n.properties_json, n.provenance_json, n.freshness_json
FROM walk
JOIN graph_nodes n ON n.id = walk.id
GROUP BY walk.id
            ),
            ranked AS (
                SELECT id, score, node_kind, label, properties_json, provenance_json, freshness_json
                FROM scored_nodes
                ORDER BY score DESC, id ASC
            ),
            kept AS (
                SELECT id, score, node_kind, label, properties_json, provenance_json, freshness_json
                FROM ranked
                LIMIT ?
            ),
            total AS (
                SELECT COUNT(*) AS cnt FROM scored_nodes
            )
            SELECT
                'meta' AS row_type,
                (SELECT cnt FROM total) AS total_discovered,
                0 AS node_id, '' AS node_kind, '' AS node_label,
                '' AS node_props, '' AS node_prov, '' AS node_fresh,
                '' AS edge_key, '' AS edge_from, '' AS edge_to, '' AS edge_kind_col,
                '' AS edge_props, '' AS edge_prov, '' AS edge_fresh
            UNION ALL
            SELECT
                'node' AS row_type,
                0 AS total_discovered,
                k.id, k.node_kind, k.label, k.properties_json, k.provenance_json, k.freshness_json,
                '' AS edge_key, '' AS edge_from, '' AS edge_to, '' AS edge_kind_col,
                '' AS edge_props, '' AS edge_prov, '' AS edge_fresh
            FROM kept k
            UNION ALL
            SELECT
                'edge' AS row_type,
                0 AS total_discovered,
                '' AS node_id, '' AS node_kind, '' AS node_label,
                '' AS node_props, '' AS node_prov, '' AS node_fresh,
                e.edge_key, e.from_id, e.to_id, e.kind, e.properties_json, e.provenance_json, e.freshness_json
            FROM graph_edges e
            WHERE EXISTS (SELECT 1 FROM kept k WHERE k.id = e.from_id)
              AND EXISTS (SELECT 1 FROM kept k2 WHERE k2.id = e.to_id)
"#,
        );
        values.push(Value::Integer(options.max_nodes.saturating_add(1) as i64));

        let mut stmt = self.conn.prepare(&sql)?;
        let mut nodes = vec![center.clone()];
        let mut edges = Vec::new();
        let mut total_discovered = 0usize;

        let rows = stmt.query_map(params_from_iter(values.iter()), |row| {
            let row_type: String = row.get(0)?;
            match row_type.as_str() {
                "meta" => Ok(QueryResult::Meta {
                    total: row.get::<_, i64>(1)? as usize,
                }),
                "node" => Ok(QueryResult::Node(node_from_row_at(row, 2)?)),
                "edge" => Ok(QueryResult::Edge(edge_from_row_at(row, 8)?)),
                _ => Err(rusqlite::Error::InvalidQuery),
            }
        })?;
        for row_result in rows {
            match row_result? {
                QueryResult::Meta { total } => {
                    total_discovered = total;
                }
                QueryResult::Node(node) => {
                    if node.id != center_id {
                        nodes.push(node);
                    }
                }
                QueryResult::Edge(edge) => {
                    edges.push(edge);
                }
            }
        }

        let total_discovered = total_discovered.max(nodes.len());
        let pruned_count = total_discovered.saturating_sub(nodes.len());

        match options.property_mode {
            PropertyMode::Full => {}
            PropertyMode::Omit => {
                for n in &mut nodes {
                    n.properties.clear();
                }
                for e in &mut edges {
                    e.properties.clear();
                }
            }
            PropertyMode::Sample => {
                let mut seen_kinds = std::collections::BTreeSet::new();
                for n in &mut nodes {
                    if !seen_kinds.contains(&n.kind) {
                        seen_kinds.insert(n.kind.clone());
                    } else {
                        n.properties.clear();
                    }
                }
                for e in &mut edges {
                    e.properties.clear();
                }
            }
        }

        Ok(Some(RankedNeighborhoodResult {
            nodes,
            edges,
            pruned_count,
            total_discovered,
        }))
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

        self.assert_not_in_temp_table_section();
        self.temp_table_active.set(true);
        let result = (|| -> Result<Option<GraphPath>> {
            let call_id = BFS_CALL_ID.fetch_add(1, Ordering::Relaxed);
            let tbl = format!("_tsift_frontier_{call_id}");

            let mut visited = BTreeSet::from([from_id.to_string()]);
            let mut parent =
                BTreeMap::<String, String>::from([(from_id.to_string(), String::new())]);
            let mut frontier = vec![from_id.to_string()];
            self.conn.execute_batch(&format!(
                r#"CREATE TEMP TABLE IF NOT EXISTS "{tbl}" (id TEXT PRIMARY KEY);
               DELETE FROM "{tbl}";"#,
            ))?;
            let select_sql = if kind.is_some() {
                format!(
                    r#"SELECT e.from_id, e.to_id
                   FROM "{tbl}" f
                   JOIN graph_edges e INDEXED BY idx_graph_edges_from_kind
                       ON e.from_id = f.id
                   WHERE e.kind = ?
                   ORDER BY e.from_id, e.to_id, e.kind"#,
                )
            } else {
                format!(
                    r#"SELECT e.from_id, e.to_id
                   FROM "{tbl}" f
                   JOIN graph_edges e INDEXED BY idx_graph_edges_from_kind
                       ON e.from_id = f.id
                   ORDER BY e.from_id, e.to_id, e.kind"#,
                )
            };
            let insert_sql = format!(r#"INSERT OR IGNORE INTO "{tbl}" (id) VALUES (?)"#);
            let delete_sql = format!(r#"DELETE FROM "{tbl}""#);
            let drop_sql = format!(r#"DROP TABLE IF EXISTS "{tbl}""#);
            let mut frontier_select_stmt = self.conn.prepare(&select_sql)?;
            let mut frontier_insert_stmt = self.conn.prepare(&insert_sql)?;
            let mut found_path: Option<GraphPath> = None;
            for _depth in 0..hop_limit {
                if frontier.is_empty() {
                    break;
                }
                self.conn.execute(&delete_sql, [])?;
                for id in &frontier {
                    frontier_insert_stmt.execute([id.as_str()])?;
                }
                let mut next_frontier = BTreeSet::new();
                let rows = if let Some(kind) = kind {
                    collect_rows(frontier_select_stmt.query_map([kind], |row| {
                        Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
                    })?)?
                } else {
                    collect_rows(frontier_select_stmt.query_map([], |row| {
                        Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
                    })?)?
                };
                for (from, next) in rows {
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
                        found_path = Some(GraphPath {
                            hops: nodes.len().saturating_sub(1),
                            nodes,
                        });
                        break;
                    }
                    next_frontier.insert(next);
                }
                if found_path.is_some() {
                    break;
                }
                frontier = next_frontier.into_iter().collect();
            }
            let _ = self.conn.execute_batch(&drop_sql);
            Ok(found_path)
        })();
        self.temp_table_active.set(false);
        result
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
            let hops = row_usize(row, 7)?;
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

    fn evidence_target_candidates(
        &self,
        target: &str,
        kinds: &[&str],
        preferred_path: Option<&str>,
    ) -> Result<Vec<GraphNode>> {
        if kinds.is_empty() {
            return Ok(Vec::new());
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
        let path_filter = if preferred_path.is_some() {
            r#"
AND EXISTS (
    SELECT 1
    FROM graph_node_properties p_path INDEXED BY idx_graph_node_properties_key_value_node
    WHERE p_path.node_id = n.id
      AND p_path.key = 'path'
      AND p_path.value = ?
)
"#
        } else {
            ""
        };
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
{path_filter}
ORDER BY CASE n.kind {kind_rank} ELSE 999 END, n.id
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
        if let Some(path) = preferred_path {
            values.push(Value::Text(path.to_string()));
        }
        values.extend(kinds.iter().map(|kind| Value::Text((*kind).to_string())));
        let mut stmt = self.conn.prepare(&sql)?;
        collect_rows(stmt.query_map(params_from_iter(values.iter()), node_from_row)?)
    }

    fn resolve_evidence_target(&self, target: &str, kinds: &[&str]) -> Result<Option<GraphNode>> {
        if let Some(node) = self.node(target)? {
            return Ok(Some(node));
        }
        Ok(self
            .evidence_target_candidates(target, kinds, None)?
            .into_iter()
            .next())
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

enum QueryResult {
    Meta { total: usize },
    Node(GraphNode),
    Edge(GraphEdge),
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
    let page_count = conn.query_row("PRAGMA page_count", [], |row| row_u64(row, 0))?;
    let page_size = conn.query_row("PRAGMA page_size", [], |row| row_u64(row, 0))?;
    Ok(page_count.saturating_mul(page_size))
}

fn sqlite_database_freelist_bytes(conn: &Connection) -> Result<u64> {
    let freelist_count = conn.query_row("PRAGMA freelist_count", [], |row| row_u64(row, 0))?;
    let page_size = conn.query_row("PRAGMA page_size", [], |row| row_u64(row, 0))?;
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
                |row| row_usize(row, 0),
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
    fn sqlite_ranked_neighborhood_prefers_recent_memory_nodes_when_pruning() {
        let store = SqliteGraphStore::in_memory().unwrap();
        store
            .upsert_node(&GraphNode::new("center", "file", "center"))
            .unwrap();
        store
            .upsert_node(&GraphNode::new("aaa-code", "symbol", "code candidate"))
            .unwrap();
        store
            .upsert_node(
                &GraphNode::new("mmm-stale", "memory_event", "stale memory")
                    .with_property("provider", "tsift-memory")
                    .with_property("observed_at_unix", "1000"),
            )
            .unwrap();
        store
            .upsert_node(
                &GraphNode::new("zzz-fresh", "memory_event", "fresh memory")
                    .with_property("provider", "tsift-memory")
                    .with_property("observed_at_unix", "1995"),
            )
            .unwrap();
        store
            .upsert_edge(&GraphEdge::new("center", "aaa-code", "mentions"))
            .unwrap();
        store
            .upsert_edge(&GraphEdge::new("center", "mmm-stale", "mentions"))
            .unwrap();
        store
            .upsert_edge(&GraphEdge::new("center", "zzz-fresh", "mentions"))
            .unwrap();

        let options = RankedNeighborhoodOptions::new(1, 1)
            .with_observed_at_now_unix(2000)
            .with_observed_at_half_life_secs(100);
        let result = store
            .ranked_neighborhood("center", &options)
            .unwrap()
            .unwrap();
        let ids: Vec<_> = result.nodes.iter().map(|node| node.id.as_str()).collect();
        assert!(ids.contains(&"center"));
        assert!(
            ids.contains(&"zzz-fresh"),
            "fresh memory node should survive pruning: {ids:?}"
        );
        assert!(!ids.contains(&"aaa-code"));
        assert!(!ids.contains(&"mmm-stale"));
    }

    #[test]
    fn sqlite_semantic_seeded_neighborhood_scores_before_sql_edge_cap() {
        let store = SqliteGraphStore::in_memory().unwrap();
        store
            .upsert_node(&GraphNode::new("seed", "semantic_concept", "graph budget"))
            .unwrap();
        store
            .upsert_node(&GraphNode::new("zzz_high", "symbol", "high_signal"))
            .unwrap();
        store
            .upsert_edge(&GraphEdge::new("zzz_high", "seed", "mentions_concept"))
            .unwrap();
        for idx in 0..24 {
            let id = format!("aaa_low_{idx:02}");
            store
                .upsert_node(&GraphNode::new(id.clone(), "note", format!("low {idx}")))
                .unwrap();
            store
                .upsert_edge(&GraphEdge::new(id, "seed", "weak_link"))
                .unwrap();
        }

        let options = SemanticSeededNeighborhoodOptions::new(1, 3)
            .with_edge_scan_cap(16)
            .with_node_discovery_cap(9);
        let result = store
            .semantic_seeded_neighborhood(&["seed".to_string()], &options)
            .unwrap();
        let ids = result
            .nodes
            .iter()
            .map(|node| node.id.as_str())
            .collect::<Vec<_>>();

        assert_eq!(ids.len(), 3);
        assert_eq!(ids[0], "seed");
        assert_eq!(ids[1], "zzz_high");
        assert_eq!(result.skipped_by_edge_cap, 9);
        assert!(result.truncated);
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
                |row| row_usize(row, 0),
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
                |row| row_usize(row, 0),
            )
            .unwrap();
        assert_eq!(edge_property_count, 1);

        let changes_before = store.conn.total_changes();
        store.upsert_projection(&projection).unwrap();
        assert_eq!(
            store.conn.total_changes(),
            changes_before,
            "unchanged projection rows should not rewrite graph rows or materialized properties"
        );
    }

    #[test]
    fn sqlite_semantic_top_candidates_use_materialized_vector_table() {
        let store = SqliteGraphStore::in_memory().unwrap();
        store
            .upsert_node(
                &GraphNode::new("concept:graph", "semantic_concept", "graph navigation")
                    .with_property("embedding_model", "fixture-v1")
                    .with_property("embedding", "1.0,0.0"),
            )
            .unwrap();
        store
            .upsert_node(
                &GraphNode::new("concept:sqlite", "semantic_concept", "sqlite search")
                    .with_property("embedding", "0.0,1.0"),
            )
            .unwrap();
        store
            .upsert_node(&GraphNode::new("entity:skip", "semantic_entity", "skipped"))
            .unwrap();

        let vector_rows: usize = store
            .conn
            .query_row(
                "SELECT COUNT(*) FROM graph_node_semantic_vectors",
                [],
                |row| row_usize(row, 0),
            )
            .unwrap();
        assert_eq!(vector_rows, 2);

        let candidates = store
            .semantic_top_candidates(&[1.0, 0.0], &["semantic_concept"], 1)
            .unwrap();
        assert_eq!(candidates.len(), 1);
        assert_eq!(candidates[0].node.id, "concept:graph");
        assert_eq!(candidates[0].score, 1.0);

        store
            .upsert_node(&GraphNode::new(
                "concept:graph",
                "semantic_concept",
                "graph navigation",
            ))
            .unwrap();
        let vector_rows_after_update: usize = store
            .conn
            .query_row(
                "SELECT COUNT(*) FROM graph_node_semantic_vectors WHERE node_id = 'concept:graph'",
                [],
                |row| row_usize(row, 0),
            )
            .unwrap();
        assert_eq!(vector_rows_after_update, 0);
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
    fn wal_checkpoint_outcome_is_recorded_not_swallowed() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("graph.db");
        let mut store = SqliteGraphStore::open(&db_path).unwrap();
        let refresh = store
            .replace_projection_with_version("root", &sample_projection(), Some("v1"), None)
            .unwrap();
        // With no concurrent reader the TRUNCATE checkpoint succeeds and the
        // outcome is recorded as a phase instead of being silently discarded.
        assert!(
            refresh
                .phase_timings
                .iter()
                .any(|phase| phase.name == "wal_checkpoint:ok"),
            "expected a wal_checkpoint:ok phase: {:?}",
            refresh
                .phase_timings
                .iter()
                .map(|phase| &phase.name)
                .collect::<Vec<_>>()
        );
    }

    #[test]
    fn wal_checkpoint_records_busy_when_a_reader_blocks_truncate() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("graph.db");
        let mut store = SqliteGraphStore::open(&db_path).unwrap();
        store
            .replace_projection_with_version("root", &sample_projection(), Some("v1"), None)
            .unwrap();

        // A second connection holds an open read transaction, pinning the WAL so
        // a TRUNCATE checkpoint cannot reset it (busy) — the path that previously
        // grew the -wal file silently (#gdbwalcheckpoint).
        let reader = rusqlite::Connection::open(&db_path).unwrap();
        reader.execute_batch("BEGIN").unwrap();
        let _pinned: i64 = reader
            .query_row("SELECT count(*) FROM graph_operator_stats", [], |row| {
                row.get(0)
            })
            .unwrap();

        let mut projection = sample_projection();
        projection
            .nodes
            .push(GraphNode::new("topic:extra", "topic", "Extra"));
        let refresh = store
            .replace_projection_with_version("root", &projection, Some("v2"), None)
            .unwrap();

        assert!(
            refresh
                .phase_timings
                .iter()
                .any(|phase| phase.name == "wal_checkpoint:busy"),
            "expected a wal_checkpoint:busy phase while a reader holds the WAL: {:?}",
            refresh
                .phase_timings
                .iter()
                .map(|phase| &phase.name)
                .collect::<Vec<_>>()
        );
        drop(reader);
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
                |row| {
                    Ok((
                        row_usize(row, 0)?,
                        row_usize(row, 1)?,
                        row_usize(row, 2)?,
                        row_usize(row, 3)?,
                    ))
                },
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
                |row| {
                    Ok((
                        row_usize(row, 0)?,
                        row_usize(row, 1)?,
                        row_usize(row, 2)?,
                        row_usize(row, 3)?,
                    ))
                },
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
                .with_property("path", "tasks/current.md")
                .with_property("handle", "backlog-handle"),
            GraphNode::new("gbak-zrefresh", "backlog", "#refresh")
                .with_property("ref_id", "refresh")
                .with_property("path", "tasks/other.md"),
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
        let by_path = store
            .evidence_target_candidates("#refresh", &["backlog"], Some("tasks/other.md"))
            .unwrap();
        assert_eq!(by_path.len(), 1);
        assert_eq!(by_path[0].id, "gbak-zrefresh");
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
            ('topic:egress', 'topic', 'Egress', '{"domain":"recording"}', '[]'),
            ('concept:graph', 'semantic_concept', 'Graph navigation', '{"embedding":"1.0,0.0","embedding_model":"fixture-v1"}', '[]');
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
                |row| row_usize(row, 0),
            )
            .unwrap();
        assert_eq!(property_rows, 2);
        let edge_property_rows: usize = store
            .conn
            .query_row(
                "SELECT COUNT(*) FROM graph_edge_properties WHERE key = 'confidence'",
                [],
                |row| row_usize(row, 0),
            )
            .unwrap();
        assert_eq!(edge_property_rows, 1);
        let semantic_vector_rows: usize = store
            .conn
            .query_row(
                "SELECT COUNT(*) FROM graph_node_semantic_vectors WHERE model = 'fixture-v1'",
                [],
                |row| row_usize(row, 0),
            )
            .unwrap();
        assert_eq!(semantic_vector_rows, 1);
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
                    && phase.detail.contains("126 unchanged skipped")
                    && phase.detail.contains("multi-row chunks up to 500 rows")),
            "{:?}",
            refresh.phase_timings
        );
        assert!(
            refresh
                .phase_timings
                .iter()
                .any(|phase| phase.name == "sqlite_node_staging"
                    && phase.detail.contains("124 unchanged skipped")),
            "{:?}",
            refresh.phase_timings
        );
        let staged_node_properties: usize = store
            .conn
            .query_row(
                "SELECT COUNT(*) FROM temp.next_graph_node_properties",
                [],
                |row| row_usize(row, 0),
            )
            .unwrap();
        let staged_edge_properties: usize = store
            .conn
            .query_row(
                "SELECT COUNT(*) FROM temp.next_graph_edge_properties",
                [],
                |row| row_usize(row, 0),
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

    #[test]
    fn sqlite_reentrant_temp_table_guard_panics() {
        let store = SqliteGraphStore::in_memory().unwrap();
        store.temp_table_active.set(true);
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            store.assert_not_in_temp_table_section();
        }));
        assert!(result.is_err());
    }

    #[test]
    fn sqlite_temp_table_guard_clears_after_method() {
        let mut store = SqliteGraphStore::in_memory().unwrap();
        let projection = GraphProjection {
            nodes: vec![],
            edges: vec![],
        };
        store.replace_projection(&projection).unwrap();
        assert!(!store.temp_table_active.get());
    }

    #[test]
    fn derive_ontology_summarizes_types_and_relations() {
        let mut store = SqliteGraphStore::in_memory().unwrap();
        let seed = GraphProjection {
            nodes: vec![
                GraphNode::new("fn:a", "function", "a"),
                GraphNode::new("fn:b", "function", "b"),
                GraphNode::new("mod:m", "module", "m"),
            ],
            edges: vec![
                GraphEdge::new("fn:a", "fn:b", "calls"),
                GraphEdge::new("mod:m", "fn:a", "contains"),
            ],
        };
        store.upsert_projection(&seed).unwrap();

        let onto = store.derive_ontology().unwrap();
        let type_kinds: std::collections::BTreeSet<_> =
            onto.nodes.iter().map(|n| n.label.clone()).collect();
        assert!(type_kinds.contains("function"));
        assert!(type_kinds.contains("module"));
        assert!(onto.nodes.iter().all(|n| n.kind == "ontology_type"));

        let rel: std::collections::BTreeSet<_> = onto
            .edges
            .iter()
            .map(|e| (e.from_id.clone(), e.kind.clone(), e.to_id.clone()))
            .collect();
        assert!(rel.contains(&(
            "ontology_type:function".into(),
            "ontology_relation:calls".into(),
            "ontology_type:function".into()
        )));
        assert!(rel.contains(&(
            "ontology_type:module".into(),
            "ontology_relation:contains".into(),
            "ontology_type:function".into()
        )));

        let function_node = onto.nodes.iter().find(|n| n.label == "function").unwrap();
        assert_eq!(function_node.properties.get("instance_count").unwrap(), "2");

        // Idempotent: upserting the ontology then re-deriving excludes ontology rows.
        store.upsert_projection(&onto).unwrap();
        let onto2 = store.derive_ontology().unwrap();
        assert!(onto2.nodes.iter().all(|n| n.kind == "ontology_type"));
        assert_eq!(onto2.nodes.len(), onto.nodes.len());
        assert_eq!(onto2.edges.len(), onto.edges.len());
    }

    /// #kgsameas: link_nodes_by_shared_property stars duplicate nodes sharing a
    /// prefixed property value to the group's smallest node id, is idempotent, and
    /// ignores values without the prefix / singleton groups.
    #[test]
    fn link_nodes_by_shared_property_stars_duplicates_idempotently() {
        let mut store = SqliteGraphStore::in_memory().unwrap();
        let seed = GraphProjection {
            nodes: vec![
                // Three nodes reconciled to one canonical entity (kgent-canon).
                GraphNode::new("kgent-z", "semantic_entity", "Dup")
                    .with_property("entity_id", "kgent-canon"),
                GraphNode::new("kgent-a", "semantic_entity", "Dup")
                    .with_property("entity_id", "kgent-canon"),
                GraphNode::new("kgent-m", "semantic_entity", "Dup")
                    .with_property("entity_id", "kgent-canon"),
                // A singleton canonical entity — no link.
                GraphNode::new("kgent-solo", "semantic_entity", "Solo")
                    .with_property("entity_id", "kgent-other"),
                // A local-slug id — not a merge key (no kgent- prefix).
                GraphNode::new("kgent-local", "semantic_entity", "Local")
                    .with_property("entity_id", "e0"),
            ],
            edges: vec![],
        };
        store.upsert_projection(&seed).unwrap();

        let written = store
            .link_nodes_by_shared_property(
                "semantic_entity",
                "entity_id",
                "kgent-",
                "same_as",
                &[("provider", "tsift-kg")],
            )
            .unwrap();
        // 3-member group -> 2 star edges; singleton + local-slug contribute none.
        assert_eq!(written, 2);

        // Representative is the smallest node id (kgent-a); members star to it.
        let same_as: Vec<(String, String)> = {
            let mut stmt = store
                .conn
                .prepare("SELECT from_id, to_id FROM graph_edges WHERE kind = 'same_as' ORDER BY from_id")
                .unwrap();
            let rows = stmt
                .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))
                .unwrap();
            rows.map(|r| r.unwrap()).collect()
        };
        assert_eq!(
            same_as,
            vec![
                ("kgent-m".to_string(), "kgent-a".to_string()),
                ("kgent-z".to_string(), "kgent-a".to_string()),
            ]
        );

        // Idempotent: a second run writes the same set, no duplicate edges.
        let written2 = store
            .link_nodes_by_shared_property(
                "semantic_entity",
                "entity_id",
                "kgent-",
                "same_as",
                &[("provider", "tsift-kg")],
            )
            .unwrap();
        assert_eq!(written2, 2);
        let edge_count: i64 = store
            .conn
            .query_row(
                "SELECT COUNT(*) FROM graph_edges WHERE kind = 'same_as'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(edge_count, 2, "re-run must not duplicate same_as edges");
    }

    /// #kgrefreshdup: delete_source_projection removes a provider's nodes for a
    /// source (cascading to edges/properties) but must NOT touch nodes from a
    /// different provider that happen to share the same `source_ref` path.
    #[test]
    fn delete_source_projection_is_scoped_by_provider_and_cascades() {
        let mut store = SqliteGraphStore::in_memory().unwrap();
        let seed = GraphProjection {
            nodes: vec![
                // KG nodes for src/a.rs.
                GraphNode::new("kg:a:1", "semantic_entity", "Alpha")
                    .with_property("provider", "tsift-kg")
                    .with_property("source_ref", "src/a.rs"),
                GraphNode::new("kg:a:2", "semantic_entity", "Beta")
                    .with_property("provider", "tsift-kg")
                    .with_property("source_ref", "src/a.rs"),
                // KG node for a different source.
                GraphNode::new("kg:b:1", "semantic_entity", "Gamma")
                    .with_property("provider", "tsift-kg")
                    .with_property("source_ref", "src/b.rs"),
                // A non-KG (e.g. AST) node sharing the src/a.rs source_ref.
                GraphNode::new("ast:a:1", "function", "fn_in_a")
                    .with_property("provider", "tsift-ast")
                    .with_property("source_ref", "src/a.rs"),
            ],
            edges: vec![
                GraphEdge::new("kg:a:1", "kg:a:2", "semantic_relation")
                    .with_property("provider", "tsift-kg"),
                GraphEdge::new("ast:a:1", "kg:a:1", "references"),
            ],
        };
        store.upsert_projection(&seed).unwrap();

        let deleted = store
            .delete_source_projection("src/a.rs", "tsift-kg")
            .unwrap();
        assert_eq!(deleted, 2, "only the two KG nodes for src/a.rs are deleted");

        // KG nodes for src/a.rs are gone.
        assert!(store.node("kg:a:1").unwrap().is_none());
        assert!(store.node("kg:a:2").unwrap().is_none());
        // KG node for the other source survives.
        assert!(store.node("kg:b:1").unwrap().is_some());
        // The non-KG node sharing the source_ref is untouched.
        assert!(store.node("ast:a:1").unwrap().is_some());

        // The semantic_relation edge between deleted nodes cascaded away; the
        // edge touching the surviving AST node + a deleted KG node also cascades.
        let edge_count: i64 = store
            .conn
            .query_row("SELECT COUNT(*) FROM graph_edges", [], |row| row.get(0))
            .unwrap();
        assert_eq!(edge_count, 0);

        // Properties for the deleted nodes cascaded too.
        let orphan_props: i64 = store
            .conn
            .query_row(
                "SELECT COUNT(*) FROM graph_node_properties WHERE node_id IN ('kg:a:1','kg:a:2')",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(orphan_props, 0);
    }
}
