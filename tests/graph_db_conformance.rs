use rusqlite::Connection;
use serde_json::{Value, json};
use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

const LIVE_CONVEX_ACCEPTANCE_ENV: &str = "TSIFT_LIVE_CONVEX_ACCEPTANCE";
const LIVE_CONVEX_GRAPH_URL_ENV: &str = "TSIFT_LIVE_CONVEX_GRAPH_URL";
const LIVE_CONVEX_AUTH_TOKEN_ENV: &str = "TSIFT_LIVE_CONVEX_AUTH_TOKEN";

fn tsift_bin() -> Command {
    Command::new(env!("CARGO_BIN_EXE_tsift"))
}

fn run_tsift(args: Vec<String>) -> Output {
    tsift_bin().args(&args).output().unwrap()
}

fn assert_tsift_json(args: Vec<String>) -> Value {
    let output = run_tsift(args.clone());
    assert!(
        output.status.success(),
        "tsift {} failed\nstdout:\n{}\nstderr:\n{}",
        args.join(" "),
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).unwrap()
}

fn assert_tsift_failure(args: Vec<String>) -> String {
    let output = run_tsift(args.clone());
    assert!(
        !output.status.success(),
        "tsift {} unexpectedly succeeded\nstdout:\n{}\nstderr:\n{}",
        args.join(" "),
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stderr).into_owned()
}

fn assert_tsift_failure_json(args: Vec<String>) -> (Value, String) {
    let output = run_tsift(args.clone());
    assert!(
        !output.status.success(),
        "tsift {} unexpectedly succeeded\nstdout:\n{}\nstderr:\n{}",
        args.join(" "),
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    (
        serde_json::from_slice(&output.stdout).unwrap(),
        String::from_utf8_lossy(&output.stderr).into_owned(),
    )
}

fn graph_db_project() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    fs::write(
        dir.path().join("main.rs"),
        r#"fn main() {
    alpha();
    bridge();
}

fn alpha() {
    beta();
    gamma();
}

fn beta() {
    gamma();
}

fn gamma() {}

fn bridge() {
    shared();
}

fn shared() {
    helper();
}

fn helper() {}
"#,
    )
    .unwrap();

    let task_dir = dir.path().join("tasks/software");
    fs::create_dir_all(&task_dir).unwrap();
    fs::write(
        task_dir.join("tsift.md"),
        r#"---
agent_doc_session: tsift-conformance
agent_doc_format: template
---

## Exchange

<!-- agent:exchange patch=append -->
### Re: setup
<!-- /agent:exchange -->

<!-- agent:queue -->
dispatch #spec-test-build-install-commit-push
- do [#gval]
<!-- /agent:queue -->

## Backlog

<!-- agent:backlog -->
- [ ] [#gval] Verify helper bridge graph-db conformance.
<!-- /agent:backlog -->
"#,
    )
    .unwrap();

    let output = run_tsift(vec![
        "index".to_string(),
        dir.path().to_string_lossy().to_string(),
    ]);
    assert!(
        output.status.success(),
        "index failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    dir
}

fn graph_db_path(project: &Path) -> PathBuf {
    project.join(".tsift/graph.db")
}

enum Backend<'a> {
    Sqlite,
    ConvexSnapshot(&'a Path),
}

fn graph_db_args(project: &Path, backend: Backend<'_>, query: Vec<String>) -> Vec<String> {
    let mut args = vec![
        "graph-db".to_string(),
        "--path".to_string(),
        project.to_string_lossy().to_string(),
        "--json".to_string(),
    ];
    if let Backend::ConvexSnapshot(snapshot) = backend {
        args.extend([
            "--backend".to_string(),
            "convex-snapshot".to_string(),
            "--convex-snapshot".to_string(),
            snapshot.to_string_lossy().to_string(),
        ]);
    }
    args.extend(query);
    args
}

fn graph_db_json(project: &Path, backend: Backend<'_>, query: Vec<String>) -> Value {
    assert_tsift_json(graph_db_args(project, backend, query))
}

fn graph_db_failure(project: &Path, backend: Backend<'_>, query: Vec<String>) -> String {
    assert_tsift_failure(graph_db_args(project, backend, query))
}

fn current_convex_snapshot(project: &Path) -> Value {
    let report = assert_tsift_json(vec![
        "convex-sync".to_string(),
        project.to_string_lossy().to_string(),
        "--json".to_string(),
    ]);
    assert_eq!(report["freshness"]["status"], "unchecked");
    json!({
        "nodes": report["node_upserts"].clone(),
        "edges": report["edge_upserts"].clone(),
    })
}

fn required_convex_indexes_json() -> Value {
    json!([
        {"table": "nodes", "name": "by_external_id", "fields": ["externalId"]},
        {"table": "nodes", "name": "by_kind", "fields": ["kind"]},
        {"table": "edges", "name": "by_edge_key", "fields": ["edgeKey"]},
        {"table": "edges", "name": "by_from_kind", "fields": ["fromExternalId", "kind"]},
        {"table": "edges", "name": "by_to_kind", "fields": ["toExternalId", "kind"]}
    ])
}

fn attach_required_convex_indexes(snapshot: &mut Value) {
    snapshot["indexes"] = required_convex_indexes_json();
}

fn write_snapshot(project: &Path, name: &str, snapshot: &Value) -> PathBuf {
    let path = project.join(name);
    fs::write(&path, serde_json::to_vec_pretty(snapshot).unwrap()).unwrap();
    path
}

fn live_convex_acceptance_enabled() -> bool {
    env::var(LIVE_CONVEX_ACCEPTANCE_ENV)
        .ok()
        .map(|value| {
            matches!(
                value.trim().to_ascii_lowercase().as_str(),
                "1" | "true" | "yes" | "on"
            )
        })
        .unwrap_or(false)
}

fn fetch_live_convex_snapshot(
    endpoint: &str,
    auth_token_env: &str,
    projection_version: &str,
) -> Value {
    let request = json!({
        "operation": "snapshot",
        "chunk": 0,
        "projectionVersion": projection_version,
        "projectionHash": null,
        "nodeRows": [],
        "edgeRows": [],
        "keys": []
    });
    let mut builder = ureq::post(endpoint);
    if let Ok(token) = env::var(auth_token_env)
        && !token.trim().is_empty()
    {
        builder = builder.header("Authorization", &format!("Bearer {token}"));
    }
    let mut response = builder
        .send_json(&request)
        .unwrap_or_else(|err| panic!("live Convex snapshot request failed for {endpoint}: {err}"));
    let response: Value = response
        .body_mut()
        .read_json()
        .unwrap_or_else(|err| panic!("live Convex snapshot response was not JSON: {err}"));
    assert_eq!(
        response["status"], "ok",
        "unexpected snapshot response: {response}"
    );
    let rows = response["rows"].clone();
    assert!(
        rows["nodes"].is_array(),
        "snapshot rows missing nodes: {rows}"
    );
    assert!(
        rows["edges"].is_array(),
        "snapshot rows missing edges: {rows}"
    );
    rows
}

fn node_ids(report: &Value) -> Vec<String> {
    report["nodes"]
        .as_array()
        .unwrap()
        .iter()
        .map(|node| node["id"].as_str().unwrap().to_string())
        .collect()
}

fn edge_keys(report: &Value) -> Vec<String> {
    report["edges"]
        .as_array()
        .unwrap()
        .iter()
        .map(|edge| {
            format!(
                "{}|{}|{}",
                edge["from_id"].as_str().unwrap(),
                edge["kind"].as_str().unwrap(),
                edge["to_id"].as_str().unwrap()
            )
        })
        .collect()
}

fn assert_sorted(values: &[String]) {
    let mut sorted = values.to_vec();
    sorted.sort();
    assert_eq!(values, sorted, "values should be deterministic and sorted");
}

fn symbol_id_by_ref(project: &Path, backend: Backend<'_>, ref_id: &str) -> String {
    let report = graph_db_json(
        project,
        backend,
        vec![
            "kind".to_string(),
            "symbol".to_string(),
            "--property".to_string(),
            format!("ref_id={ref_id}"),
            "--limit".to_string(),
            "1".to_string(),
        ],
    );
    let ids = node_ids(&report);
    assert_eq!(
        ids.len(),
        1,
        "expected one symbol id for {ref_id}: {report}"
    );
    ids[0].clone()
}

fn sql_node_ids(db_path: &Path, kind: &str) -> Vec<String> {
    let conn = Connection::open(db_path).unwrap();
    let mut stmt = conn
        .prepare("SELECT id FROM graph_nodes WHERE kind = ?1 ORDER BY id")
        .unwrap();
    stmt.query_map([kind], |row| row.get::<_, String>(0))
        .unwrap()
        .collect::<Result<Vec<_>, _>>()
        .unwrap()
}

fn assert_graph_db_snapshot_query_parity(project: &Path, snapshot: &Path) {
    let sqlite_schema = graph_db_json(project, Backend::Sqlite, vec!["schema".to_string()]);
    let convex_schema = graph_db_json(
        project,
        Backend::ConvexSnapshot(snapshot),
        vec!["schema".to_string()],
    );
    assert_eq!(sqlite_schema["schema"], convex_schema["schema"]);

    let first_query = vec![
        "kind".to_string(),
        "symbol".to_string(),
        "--property".to_string(),
        "path=main.rs".to_string(),
        "--limit".to_string(),
        "2".to_string(),
    ];
    let sqlite_first = graph_db_json(project, Backend::Sqlite, first_query.clone());
    let convex_first = graph_db_json(project, Backend::ConvexSnapshot(snapshot), first_query);
    let sqlite_first_ids = node_ids(&sqlite_first);
    assert_eq!(sqlite_first_ids, node_ids(&convex_first));
    assert_sorted(&sqlite_first_ids);
    assert_eq!(sqlite_first["page"], convex_first["page"]);
    assert!(sqlite_first["page"]["truncated"].as_bool().unwrap());

    let cursor = sqlite_first["page"]["next_cursor"].as_str().unwrap();
    let second_query = vec![
        "kind".to_string(),
        "symbol".to_string(),
        "--property".to_string(),
        "path=main.rs".to_string(),
        "--cursor".to_string(),
        cursor.to_string(),
        "--limit".to_string(),
        "2".to_string(),
    ];
    let sqlite_second = graph_db_json(project, Backend::Sqlite, second_query.clone());
    let convex_second = graph_db_json(project, Backend::ConvexSnapshot(snapshot), second_query);
    let sqlite_second_ids = node_ids(&sqlite_second);
    assert_eq!(sqlite_second_ids, node_ids(&convex_second));
    assert_sorted(&sqlite_second_ids);
    assert_eq!(sqlite_second["page"], convex_second["page"]);

    let main_id = symbol_id_by_ref(project, Backend::Sqlite, "main");
    let helper_id = symbol_id_by_ref(project, Backend::Sqlite, "helper");
    assert_eq!(
        main_id,
        symbol_id_by_ref(project, Backend::ConvexSnapshot(snapshot), "main")
    );
    assert_eq!(
        helper_id,
        symbol_id_by_ref(project, Backend::ConvexSnapshot(snapshot), "helper")
    );

    let node_query = vec!["node".to_string(), main_id.clone()];
    let sqlite_node = graph_db_json(project, Backend::Sqlite, node_query.clone());
    let convex_node = graph_db_json(project, Backend::ConvexSnapshot(snapshot), node_query);
    assert_eq!(sqlite_node["node"], convex_node["node"]);

    let path_query = vec![
        "path".to_string(),
        main_id.clone(),
        helper_id,
        "--edge-kind".to_string(),
        "calls".to_string(),
    ];
    let sqlite_path = graph_db_json(project, Backend::Sqlite, path_query.clone());
    let convex_path = graph_db_json(project, Backend::ConvexSnapshot(snapshot), path_query);
    assert_eq!(sqlite_path["path"], convex_path["path"]);
    assert!(sqlite_path["path"]["hops"].as_u64().unwrap() >= 1);

    let neighborhood_query = vec![
        "neighborhood".to_string(),
        main_id,
        "--depth".to_string(),
        "3".to_string(),
        "--edge-kind".to_string(),
        "calls".to_string(),
        "--limit".to_string(),
        "20".to_string(),
    ];
    let sqlite_neighborhood = graph_db_json(project, Backend::Sqlite, neighborhood_query.clone());
    let convex_neighborhood = graph_db_json(
        project,
        Backend::ConvexSnapshot(snapshot),
        neighborhood_query,
    );
    assert_eq!(
        node_ids(&sqlite_neighborhood),
        node_ids(&convex_neighborhood)
    );
    let sqlite_edges = edge_keys(&sqlite_neighborhood);
    assert_eq!(sqlite_edges, edge_keys(&convex_neighborhood));
    assert!(!sqlite_edges.is_empty());
    assert_eq!(sqlite_neighborhood["page"], convex_neighborhood["page"]);

    let repeated_sqlite_neighborhood = graph_db_json(
        project,
        Backend::Sqlite,
        vec![
            "neighborhood".to_string(),
            symbol_id_by_ref(project, Backend::Sqlite, "main"),
            "--depth".to_string(),
            "3".to_string(),
            "--edge-kind".to_string(),
            "calls".to_string(),
            "--limit".to_string(),
            "20".to_string(),
        ],
    );
    assert_eq!(sqlite_edges, edge_keys(&repeated_sqlite_neighborhood));
}

#[test]
fn graph_db_cli_conformance_matches_sqlite_and_convex_snapshot_queries() {
    let project = graph_db_project();
    let snapshot_value = current_convex_snapshot(project.path());
    let snapshot = write_snapshot(project.path(), "convex-current.json", &snapshot_value);

    assert_graph_db_snapshot_query_parity(project.path(), &snapshot);
}

#[test]
#[ignore = "requires a dedicated live Convex deployment"]
fn live_convex_graph_backend_acceptance_applies_and_matches_graph_db_queries() {
    if !live_convex_acceptance_enabled() {
        eprintln!(
            "skipping live Convex acceptance; set {LIVE_CONVEX_ACCEPTANCE_ENV}=1 and {LIVE_CONVEX_GRAPH_URL_ENV}=https://<deployment>.convex.site/tsift/graph"
        );
        return;
    }
    let endpoint = env::var(LIVE_CONVEX_GRAPH_URL_ENV).unwrap_or_else(|_| {
        panic!("{LIVE_CONVEX_GRAPH_URL_ENV} must be set when {LIVE_CONVEX_ACCEPTANCE_ENV}=1")
    });
    let project = graph_db_project();

    let output = run_tsift(vec![
        "convex-sync".to_string(),
        project.path().to_string_lossy().to_string(),
        "--remote-snapshot".to_string(),
        "--apply".to_string(),
        "--endpoint".to_string(),
        endpoint.clone(),
        "--auth-token-env".to_string(),
        LIVE_CONVEX_AUTH_TOKEN_ENV.to_string(),
        "--json".to_string(),
    ]);
    assert!(
        output.status.success(),
        "live convex-sync apply failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let apply_report: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(apply_report["dry_run"], false, "{apply_report}");
    assert_eq!(
        apply_report["transport"]["remote_snapshot"], true,
        "{apply_report}"
    );
    assert!(
        apply_report["diagnostics"]
            .as_array()
            .unwrap()
            .iter()
            .any(|diagnostic| diagnostic
                .as_str()
                .unwrap()
                .contains("live Convex transport completed")),
        "{apply_report}"
    );

    let projection_version = apply_report["projection_version"].as_str().unwrap();
    let live_snapshot =
        fetch_live_convex_snapshot(&endpoint, LIVE_CONVEX_AUTH_TOKEN_ENV, projection_version);
    let snapshot = write_snapshot(project.path(), "convex-live-current.json", &live_snapshot);

    let doctor = graph_db_json(
        project.path(),
        Backend::ConvexSnapshot(&snapshot),
        vec!["doctor".to_string()],
    );
    assert_eq!(doctor["status"], "ok", "{doctor}");
    assert_graph_db_snapshot_query_parity(project.path(), &snapshot);
}

#[test]
fn graph_db_cli_fails_closed_for_newer_sqlite_schema() {
    let project = graph_db_project();
    graph_db_json(project.path(), Backend::Sqlite, vec!["schema".to_string()]);

    let conn = Connection::open(graph_db_path(project.path())).unwrap();
    conn.pragma_update(None, "user_version", 999).unwrap();
    drop(conn);

    let stderr = graph_db_failure(project.path(), Backend::Sqlite, vec!["schema".to_string()]);
    assert!(
        stderr.contains("schema version 999 is newer than supported"),
        "{stderr}"
    );
}

#[test]
fn graph_db_cli_rolls_back_failed_sqlite_refresh() {
    let project = graph_db_project();
    graph_db_json(
        project.path(),
        Backend::Sqlite,
        vec!["kind".to_string(), "symbol".to_string()],
    );
    let db_path = graph_db_path(project.path());
    let before = sql_node_ids(&db_path, "symbol");
    assert!(!before.is_empty());

    let conn = Connection::open(&db_path).unwrap();
    conn.execute_batch(
        r#"
        CREATE TRIGGER fail_graph_edge_insert
        BEFORE INSERT ON graph_edges
        BEGIN
            SELECT RAISE(ABORT, 'forced graph edge insert failure');
        END;
        "#,
    )
    .unwrap();
    drop(conn);

    let stderr = graph_db_failure(
        project.path(),
        Backend::Sqlite,
        vec!["kind".to_string(), "symbol".to_string()],
    );
    assert!(
        stderr.contains("forced graph edge insert failure"),
        "{stderr}"
    );
    assert_eq!(sql_node_ids(&db_path, "symbol"), before);
}

#[test]
fn convex_sync_cli_orders_tombstone_reconciliation_chunks() {
    let project = graph_db_project();
    let mut snapshot = current_convex_snapshot(project.path());
    snapshot["nodes"].as_array_mut().unwrap().push(json!({
        "externalId": "stale-node",
        "kind": "backlog",
        "label": "stale",
        "properties": {},
        "provenance": []
    }));
    snapshot["edges"].as_array_mut().unwrap().push(json!({
        "edgeKey": "stale-edge",
        "fromExternalId": "stale-node",
        "toExternalId": "stale-node",
        "kind": "mentions",
        "properties": {},
        "provenance": []
    }));
    let snapshot_path = write_snapshot(project.path(), "convex-stale-extra.json", &snapshot);

    let report = assert_tsift_json(vec![
        "convex-sync".to_string(),
        project.path().to_string_lossy().to_string(),
        "--snapshot".to_string(),
        snapshot_path.to_string_lossy().to_string(),
        "--chunk-size".to_string(),
        "1".to_string(),
        "--json".to_string(),
    ]);
    assert_eq!(report["edge_tombstones"], json!(["stale-edge"]));
    assert_eq!(report["node_tombstones"], json!(["stale-node"]));

    let operations = report["chunks"]
        .as_array()
        .unwrap()
        .iter()
        .map(|chunk| chunk["operation"].as_str().unwrap().to_string())
        .collect::<Vec<_>>();
    assert_eq!(operations, vec!["delete_edges", "delete_nodes"]);
    assert!(
        report["diagnostics"]
            .as_array()
            .unwrap()
            .iter()
            .any(|diagnostic| diagnostic
                .as_str()
                .unwrap()
                .contains("edge tombstones before node tombstones"))
    );
}

#[test]
fn graph_db_cli_fails_closed_for_stale_convex_snapshot() {
    let project = graph_db_project();
    let mut snapshot = current_convex_snapshot(project.path());
    let nodes = snapshot["nodes"].as_array_mut().unwrap();
    let meta_index = nodes
        .iter()
        .position(|node| node["kind"] == "projection_meta")
        .unwrap();
    nodes.remove(meta_index);
    let snapshot_path = write_snapshot(project.path(), "convex-missing-meta.json", &snapshot);

    let stderr = graph_db_failure(
        project.path(),
        Backend::ConvexSnapshot(&snapshot_path),
        vec!["schema".to_string()],
    );
    assert!(
        stderr.contains("graph database read failed closed for convex-snapshot backend"),
        "{stderr}"
    );
    assert!(stderr.contains("projection hash mismatch"), "{stderr}");
}

#[test]
fn graph_db_cli_rejects_convex_snapshot_orphan_edges() {
    let project = graph_db_project();
    let mut snapshot = current_convex_snapshot(project.path());
    let target_id = snapshot["nodes"]
        .as_array()
        .unwrap()
        .iter()
        .find(|node| node["kind"] == "symbol")
        .unwrap()["externalId"]
        .as_str()
        .unwrap()
        .to_string();
    snapshot["edges"].as_array_mut().unwrap().push(json!({
        "edgeKey": "edge:orphan",
        "fromExternalId": "missing-symbol",
        "toExternalId": target_id,
        "kind": "calls",
        "properties": {},
        "provenance": []
    }));
    let snapshot_path = write_snapshot(project.path(), "convex-orphan-edge.json", &snapshot);

    let stderr = graph_db_failure(
        project.path(),
        Backend::ConvexSnapshot(&snapshot_path),
        vec!["schema".to_string()],
    );
    assert!(
        stderr.contains("Convex snapshot edge edge:orphan references missing from node"),
        "{stderr}"
    );
}

#[test]
fn graph_db_doctor_passes_for_current_sqlite_and_convex_snapshot() {
    let project = graph_db_project();
    let mut snapshot = current_convex_snapshot(project.path());
    attach_required_convex_indexes(&mut snapshot);
    let snapshot_path = write_snapshot(project.path(), "convex-current-indexed.json", &snapshot);

    let sqlite = graph_db_json(project.path(), Backend::Sqlite, vec!["doctor".to_string()]);
    assert_eq!(sqlite["status"], "ok");
    assert_eq!(sqlite["fail_closed"], false);
    assert!(
        sqlite["checks"]
            .as_array()
            .unwrap()
            .iter()
            .all(|check| check["status"] == "ok"),
        "{sqlite}"
    );

    let convex = graph_db_json(
        project.path(),
        Backend::ConvexSnapshot(&snapshot_path),
        vec!["doctor".to_string()],
    );
    assert_eq!(convex["status"], "ok");
    assert_eq!(convex["fail_closed"], false);
    assert!(
        convex["checks"]
            .as_array()
            .unwrap()
            .iter()
            .all(|check| check["status"] == "ok"),
        "{convex}"
    );
}

#[test]
fn graph_db_doctor_fails_closed_for_sqlite_stale_metadata_and_schema_drift() {
    let project = graph_db_project();
    current_convex_snapshot(project.path());
    let db_path = graph_db_path(project.path());
    let conn = Connection::open(&db_path).unwrap();
    conn.execute(
        "UPDATE graph_projection_versions SET projection_version = 'old-v0', content_hash = NULL WHERE scope = 'root'",
        [],
    )
    .unwrap();
    conn.execute("DROP INDEX idx_graph_edges_to_kind", [])
        .unwrap();
    drop(conn);

    let (report, stderr) = assert_tsift_failure_json(graph_db_args(
        project.path(),
        Backend::Sqlite,
        vec!["doctor".to_string()],
    ));
    assert_eq!(report["status"], "fail_closed");
    assert_eq!(report["fail_closed"], true);
    let report_text = serde_json::to_string(&report).unwrap();
    assert!(
        report_text.contains("projection version mismatch"),
        "{report_text}"
    );
    assert!(
        report_text.contains("projection content hash is missing"),
        "{report_text}"
    );
    assert!(
        report_text.contains("missing index idx_graph_edges_to_kind"),
        "{report_text}"
    );
    assert!(
        report["repair_commands"]
            .as_array()
            .unwrap()
            .iter()
            .any(|command| command.as_str().unwrap().contains("tsift traverse")),
        "{report}"
    );
    assert!(stderr.contains("graph-db doctor failed closed"), "{stderr}");
}

#[test]
fn graph_db_doctor_fails_closed_for_convex_index_duplicates_and_orphans() {
    let project = graph_db_project();
    let mut snapshot = current_convex_snapshot(project.path());
    let mut indexes = required_convex_indexes_json();
    indexes.as_array_mut().unwrap().pop();
    snapshot["indexes"] = indexes;
    let duplicate_node = snapshot["nodes"].as_array().unwrap()[0].clone();
    snapshot["nodes"]
        .as_array_mut()
        .unwrap()
        .push(duplicate_node);
    let target_id = snapshot["nodes"]
        .as_array()
        .unwrap()
        .iter()
        .find(|node| node["kind"] == "symbol")
        .unwrap()["externalId"]
        .as_str()
        .unwrap()
        .to_string();
    snapshot["edges"].as_array_mut().unwrap().push(json!({
        "edgeKey": "edge:orphan",
        "fromExternalId": "missing-symbol",
        "toExternalId": target_id,
        "kind": "calls",
        "properties": {},
        "provenance": []
    }));
    let snapshot_path = write_snapshot(project.path(), "convex-doctor-bad.json", &snapshot);

    let (report, stderr) = assert_tsift_failure_json(graph_db_args(
        project.path(),
        Backend::ConvexSnapshot(&snapshot_path),
        vec!["doctor".to_string()],
    ));
    assert_eq!(report["status"], "fail_closed");
    let report_text = serde_json::to_string(&report).unwrap();
    assert!(
        report_text.contains("duplicate node externalId"),
        "{report_text}"
    );
    assert!(
        report_text.contains("references missing from node"),
        "{report_text}"
    );
    assert!(
        report_text.contains("missing required index metadata"),
        "{report_text}"
    );
    assert!(
        report["repair_commands"]
            .as_array()
            .unwrap()
            .iter()
            .any(|command| command.as_str().unwrap().contains("convex-sync")),
        "{report}"
    );
    assert!(stderr.contains("graph-db doctor failed closed"), "{stderr}");
}
