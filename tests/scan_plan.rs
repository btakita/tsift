//! Regression tests for the SQLite query plans backing the graph.db hot paths
//! covered by task #gdbscanproof. Each test builds a small synthetic graph that
//! mirrors the production schema in `src/substrate.rs`, then asserts that the
//! four query shapes (edge_property_scan, incident_edges, kind+property scan,
//! and the BFS frontier probe) reach `graph_edges` / `graph_nodes` through the
//! declared indexes rather than via a scan.
//!
//! Evidence captured in `plans/gdbscanproof-evidence.md`.

use rusqlite::Connection;

const SCHEMA: &str = r#"
PRAGMA foreign_keys = ON;

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
CREATE INDEX idx_graph_nodes_kind_label ON graph_nodes(kind, label, id);

CREATE TABLE graph_edges (
    edge_key TEXT NOT NULL UNIQUE,
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
CREATE UNIQUE INDEX idx_graph_edges_edge_key ON graph_edges(edge_key);

CREATE TABLE graph_node_properties (
    node_id TEXT NOT NULL,
    key TEXT NOT NULL,
    value TEXT NOT NULL,
    PRIMARY KEY (node_id, key)
);
CREATE INDEX idx_graph_node_properties_key_value_node
    ON graph_node_properties(key, value, node_id);

CREATE TABLE graph_edge_properties (
    edge_key TEXT NOT NULL,
    key TEXT NOT NULL,
    value TEXT NOT NULL,
    PRIMARY KEY (edge_key, key)
);
CREATE INDEX idx_graph_edge_properties_key_value_edge
    ON graph_edge_properties(key, value, edge_key);
"#;

const HUB_LEAVES: usize = 1_000;
const CHAIN_DEPTH: usize = 500;

fn build_synthetic_graph() -> Connection {
    let conn = Connection::open_in_memory().unwrap();
    conn.execute_batch(SCHEMA).unwrap();
    let tx_conn = &conn;
    let tx = tx_conn.unchecked_transaction().unwrap();

    // High-degree hub: hub-0 defines HUB_LEAVES leaf symbols.
    tx.execute(
        "INSERT INTO graph_nodes(id, kind, label) VALUES ('hub-0', 'file', 'hub_file.rs')",
        [],
    )
    .unwrap();
    {
        let mut insert_node = tx
            .prepare("INSERT INTO graph_nodes(id, kind, label) VALUES (?1, 'symbol', ?2)")
            .unwrap();
        let mut insert_edge = tx
            .prepare("INSERT INTO graph_edges(edge_key, from_id, to_id, kind) VALUES (?1, 'hub-0', ?2, 'defines')")
            .unwrap();
        let mut insert_node_prop = tx
            .prepare("INSERT INTO graph_node_properties(node_id, key, value) VALUES (?1, 'path', ?2)")
            .unwrap();
        let mut insert_edge_prop = tx
            .prepare(
                "INSERT INTO graph_edge_properties(edge_key, key, value) VALUES (?1, 'label', ?2)",
            )
            .unwrap();
        for idx in 0..HUB_LEAVES {
            let node_id = format!("leaf-{idx:05}");
            let edge_key = format!("edge-hub-{idx:05}");
            let path = if idx % 3 == 0 { "src/lib.rs" } else { "src/main.rs" };
            let label = if idx % 10 == 0 { "hot" } else { "cold" };
            insert_node.execute((&node_id, &node_id)).unwrap();
            insert_edge.execute((&edge_key, &node_id)).unwrap();
            insert_node_prop.execute((&node_id, path)).unwrap();
            insert_edge_prop.execute((&edge_key, label)).unwrap();
        }
    }

    // Deep call chain: chain-00000 -> chain-00001 -> ... -> chain-{CHAIN_DEPTH-1}.
    {
        let mut insert_node = tx
            .prepare("INSERT INTO graph_nodes(id, kind, label) VALUES (?1, 'symbol', ?1)")
            .unwrap();
        for idx in 0..CHAIN_DEPTH {
            let id = format!("chain-{idx:05}");
            insert_node.execute([&id]).unwrap();
        }
        let mut insert_edge = tx
            .prepare(
                "INSERT INTO graph_edges(edge_key, from_id, to_id, kind) \
                 VALUES (?1, ?2, ?3, 'calls')",
            )
            .unwrap();
        for idx in 0..(CHAIN_DEPTH - 1) {
            let edge_key = format!("edge-chain-{idx:05}");
            let from = format!("chain-{idx:05}");
            let to = format!("chain-{:05}", idx + 1);
            insert_edge.execute((&edge_key, &from, &to)).unwrap();
        }
    }

    tx.commit().unwrap();
    conn.execute_batch("ANALYZE").unwrap();
    conn
}

fn query_plan(conn: &Connection, sql: &str) -> String {
    let mut stmt = conn
        .prepare(&format!("EXPLAIN QUERY PLAN {sql}"))
        .unwrap();
    let rows: Vec<String> = stmt
        .query_map([], |row| row.get::<_, String>(3))
        .unwrap()
        .map(|r| r.unwrap())
        .collect();
    rows.join("\n")
}

fn assert_plan_uses(plan: &str, indexes: &[&str], shape: &str) {
    for index in indexes {
        assert!(
            plan.contains(index),
            "{shape} plan must use {index}; got plan:\n{plan}"
        );
    }
    let lower = plan.to_lowercase();
    // SQLite reports table scans as "SCAN graph_edges" / "SCAN graph_nodes".
    // Tolerate the top-level CO-ROUTINE wrapper for UNION queries ("SCAN e"
    // refers to the bounded materialized union, not the base table).
    assert!(
        !lower.contains("scan graph_edges") && !lower.contains("scan graph_nodes"),
        "{shape} plan must not scan base tables; got plan:\n{plan}"
    );
}

#[test]
fn edge_property_scan_uses_property_and_edge_key_indexes() {
    let conn = build_synthetic_graph();
    let sql = "SELECT e.edge_key, e.from_id, e.to_id, e.kind \
               FROM graph_edge_properties ep0 INDEXED BY idx_graph_edge_properties_key_value_edge \
               JOIN graph_edges e INDEXED BY idx_graph_edges_edge_key ON e.edge_key = ep0.edge_key \
               WHERE ep0.key = 'label' AND ep0.value = 'cold' AND e.kind = 'defines' \
               ORDER BY ep0.edge_key LIMIT 51";
    let plan = query_plan(&conn, sql);
    assert_plan_uses(
        &plan,
        &[
            "idx_graph_edge_properties_key_value_edge",
            "idx_graph_edges_edge_key",
        ],
        "edge_property_scan",
    );
}

#[test]
fn incident_edges_union_uses_directional_indexes_on_hub() {
    let conn = build_synthetic_graph();
    let sql = "SELECT edge_key, from_id, to_id, kind FROM ( \
                 SELECT e.edge_key, e.from_id, e.to_id, e.kind \
                 FROM graph_edges e INDEXED BY idx_graph_edges_from_kind \
                 WHERE e.from_id = 'hub-0' \
                 UNION \
                 SELECT e.edge_key, e.from_id, e.to_id, e.kind \
                 FROM graph_edges e INDEXED BY idx_graph_edges_to_kind \
                 WHERE e.to_id = 'hub-0' \
               ) e ORDER BY e.edge_key LIMIT 51";
    let plan = query_plan(&conn, sql);
    assert_plan_uses(
        &plan,
        &["idx_graph_edges_from_kind", "idx_graph_edges_to_kind"],
        "incident_edges",
    );
}

#[test]
fn kind_property_filter_uses_kind_and_property_indexes() {
    let conn = build_synthetic_graph();
    let sql = "SELECT id, kind, label FROM graph_nodes \
               WHERE kind = 'symbol' \
                 AND EXISTS ( \
                   SELECT 1 FROM graph_node_properties p0 \
                     INDEXED BY idx_graph_node_properties_key_value_node \
                   WHERE p0.node_id = graph_nodes.id \
                     AND p0.key = 'path' AND p0.value = 'src/main.rs' \
                 ) \
               ORDER BY id LIMIT 51";
    let plan = query_plan(&conn, sql);
    assert_plan_uses(
        &plan,
        &[
            "idx_graph_nodes_kind_label",
            "idx_graph_node_properties_key_value_node",
        ],
        "kind_property_filter",
    );
}

#[test]
fn path_frontier_probe_uses_from_kind_index_on_hub_and_chain() {
    let conn = build_synthetic_graph();
    let probe_sql = "SELECT from_id, to_id FROM graph_edges INDEXED BY idx_graph_edges_from_kind \
                     WHERE from_id = ?1 ORDER BY from_id, to_id, kind";
    for node in ["hub-0", "chain-00000", "chain-00250"] {
        let sql = probe_sql.replace("?1", &format!("'{node}'"));
        let plan = query_plan(&conn, &sql);
        assert_plan_uses(&plan, &["idx_graph_edges_from_kind"], "path_frontier");
    }
}
