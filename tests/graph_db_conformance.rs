use rusqlite::Connection;
use serde_json::{Value, json};
use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::time::{Duration, Instant};

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
    fs::write(
        dir.path().join("isolated.rs"),
        r#"pub fn independent_worker() {
    isolated_leaf();
}

fn isolated_leaf() {}
"#,
    )
    .unwrap();
    fs::write(
        dir.path().join("closure.rs"),
        r#"pub fn closure_worker() {
    closure_leaf();
}

fn closure_leaf() {}
"#,
    )
    .unwrap();
    fs::write(
        dir.path().join("clean.rs"),
        r#"pub fn clean_worker() {
    clean_leaf();
}

fn clean_leaf() {}
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
Completed `#gval`; touched files `main.rs`; tests `cargo test --test graph_db_conformance`; follow-up `#solo`.
Completed `#solo`; touched files `isolated.rs`; tests `cargo test --test graph_db_conformance`; follow-up `#gval`.
Blocked `#shrd`; touched files `main.rs`; tests `cargo test --test graph_db_conformance`; follow-up `#gval`.
Blocked `#shrd`; touched files `main.rs`; tests `cargo test --test graph_db_conformance`; follow-up `#solo`.
Blocked `#wfdb`; touched files `closure.rs`; tests `cargo test --test retired_closure`; follow-up `#shrd`.
Completed `#wfok`; touched files `clean.rs`.
<!-- /agent:exchange -->

	<!-- agent:queue -->
	dispatch #spec-test-build-install-commit-push
	- do [#gval]
	- do [#shrd]
		- do [#solo]
	- do [#wfdb]
	- do [#wfok]
		<!-- /agent:queue -->

		## Backlog

		<!-- agent:backlog -->
	- [ ] [#gval] Verify helper bridge graph-db conformance.
	- [ ] [#shrd] Adjust shared helper ownership in main module.
	- [ ] [#solo] Update independent worker fixture in isolated module.
	- [ ] [#wfdb] Refresh closure worker feedback debt in closure module.
	- [ ] [#wfok] Update clean worker result fixture in clean module.
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

fn init_git_repo(path: &Path) {
    let status = Command::new("git")
        .args(["init"])
        .current_dir(path)
        .status()
        .unwrap();
    assert!(status.success(), "git init failed");

    let status = Command::new("git")
        .args(["add", "."])
        .current_dir(path)
        .status()
        .unwrap();
    assert!(status.success(), "git add failed");

    let status = Command::new("git")
        .args([
            "-c",
            "user.name=tsift-tests",
            "-c",
            "user.email=tsift-tests@example.com",
            "commit",
            "--quiet",
            "-m",
            "init",
        ])
        .current_dir(path)
        .status()
        .unwrap();
    assert!(status.success(), "git commit failed");
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

fn graph_orchestration_contract_fixture() -> Value {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("fixtures/graph-db-operator-examples/graph-orchestration-contracts.json");
    serde_json::from_slice(&fs::read(path).unwrap()).unwrap()
}

fn agent_orchestration_acceptance_pack_fixture() -> Value {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("fixtures/graph-db-operator-examples/agent-orchestration-acceptance-pack.json");
    serde_json::from_slice(&fs::read(path).unwrap()).unwrap()
}

fn contract_entry<'a>(fixture: &'a Value, name: &str) -> &'a Value {
    fixture["contracts"]
        .as_array()
        .unwrap()
        .iter()
        .find(|entry| entry["name"] == name)
        .unwrap_or_else(|| panic!("missing contract fixture entry {name}: {fixture}"))
}

fn json_path_exists(value: &Value, path: &str) -> bool {
    fn step(value: &Value, parts: &[&str]) -> bool {
        if parts.is_empty() {
            return !value.is_null();
        }
        match value {
            Value::Array(items) => items.iter().any(|item| step(item, parts)),
            Value::Object(map) => map
                .get(parts[0])
                .is_some_and(|next| step(next, &parts[1..])),
            _ => false,
        }
    }
    let parts = path.split('.').collect::<Vec<_>>();
    step(value, &parts)
}

fn assert_contract_fields(fixture: &Value, name: &str, report: &Value) {
    let contract = contract_entry(fixture, name);
    assert_eq!(
        report["contract_version"], contract["version"],
        "{name} version mismatch: {report}"
    );
    for field in contract["required_fields"].as_array().unwrap() {
        let field = field.as_str().unwrap();
        assert!(
            json_path_exists(report, field),
            "{name} missing required field {field}: {report}"
        );
    }
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
fn graph_db_cli_covers_agent_loop_workspace_fixture_rows() {
    let project = graph_db_project();
    assert_tsift_json(vec![
        "traverse".to_string(),
        "--path".to_string(),
        project.path().to_string_lossy().to_string(),
    ]);

    let session = graph_db_json(
        project.path(),
        Backend::Sqlite,
        vec![
            "kind".to_string(),
            "session".to_string(),
            "--property".to_string(),
            "ref_id=tsift-conformance".to_string(),
            "--limit".to_string(),
            "1".to_string(),
        ],
    );
    let session_id = node_ids(&session).remove(0);

    let backlog = graph_db_json(
        project.path(),
        Backend::Sqlite,
        vec![
            "kind".to_string(),
            "backlog".to_string(),
            "--property".to_string(),
            "ref_id=gval".to_string(),
            "--limit".to_string(),
            "1".to_string(),
        ],
    );
    let backlog_id = node_ids(&backlog).remove(0);

    let source = graph_db_json(
        project.path(),
        Backend::Sqlite,
        vec![
            "kind".to_string(),
            "source_handle".to_string(),
            "--property".to_string(),
            "file=main.rs".to_string(),
            "--limit".to_string(),
            "1".to_string(),
        ],
    );
    let source_id = node_ids(&source).remove(0);

    let worker = graph_db_json(
        project.path(),
        Backend::Sqlite,
        vec![
            "kind".to_string(),
            "worker_context".to_string(),
            "--property".to_string(),
            "target=tasks/software/tsift.md".to_string(),
            "--limit".to_string(),
            "1".to_string(),
        ],
    );
    assert_eq!(node_ids(&worker).len(), 1);

    let neighborhood = graph_db_json(
        project.path(),
        Backend::Sqlite,
        vec![
            "neighborhood".to_string(),
            session_id,
            "--depth".to_string(),
            "3".to_string(),
            "--limit".to_string(),
            "50".to_string(),
        ],
    );
    let neighborhood_ids = node_ids(&neighborhood);
    assert!(neighborhood_ids.contains(&backlog_id), "{neighborhood}");
    assert!(
        neighborhood["nodes"]
            .as_array()
            .unwrap()
            .iter()
            .any(|node| node["kind"] == "worker_context"),
        "{neighborhood}"
    );

    let path = graph_db_json(
        project.path(),
        Backend::Sqlite,
        vec![
            "path".to_string(),
            backlog_id,
            source_id,
            "--max-hops".to_string(),
            "3".to_string(),
        ],
    );
    assert_eq!(path["path"]["hops"], 2, "{path}");
}

#[test]
fn graph_db_refresh_and_status_materialize_operator_report() {
    let project = graph_db_project();

    let initial = graph_db_json(project.path(), Backend::Sqlite, vec!["status".to_string()]);
    assert_eq!(initial["status"], "missing", "{initial}");
    assert_eq!(initial["materialized"], false, "{initial}");
    assert!(
        initial["next_commands"]
            .as_array()
            .unwrap()
            .iter()
            .any(|command| command.as_str().unwrap().contains("graph-db")
                && command.as_str().unwrap().contains("refresh")),
        "{initial}"
    );

    let refresh = graph_db_json(project.path(), Backend::Sqlite, vec!["refresh".to_string()]);
    assert_eq!(refresh["operation"], "refresh", "{refresh}");
    assert_eq!(refresh["status"], "current", "{refresh}");
    assert_eq!(refresh["materialized"], true, "{refresh}");
    assert_eq!(
        refresh["freshness"]["projection_version"], "tsift-traversal-v1",
        "{refresh}"
    );
    assert!(refresh["freshness"]["content_hash"].as_str().is_some());
    assert!(refresh["freshness"]["source_watermark"].as_str().is_some());
    assert!(refresh["counts"]["nodes"].as_u64().unwrap() > 0);
    assert!(refresh["counts"]["edges"].as_u64().unwrap() > 0);
    assert!(refresh["counts"]["tombstones"]["total"].as_u64().is_some());
    let refresh_commands = refresh["next_commands"].as_array().unwrap();
    assert!(
        refresh_commands
            .iter()
            .any(|command| command.as_str().unwrap().contains("doctor")),
        "{refresh}"
    );
    assert!(
        refresh_commands
            .iter()
            .any(|command| command.as_str().unwrap().contains("drift")),
        "{refresh}"
    );
    assert!(
        refresh_commands
            .iter()
            .any(|command| command.as_str().unwrap().contains("convex-sync")),
        "{refresh}"
    );

    let status = graph_db_json(project.path(), Backend::Sqlite, vec!["status".to_string()]);
    assert_eq!(status["operation"], "status", "{status}");
    assert_eq!(status["status"], "current", "{status}");
    assert_eq!(status["counts"]["nodes"], refresh["counts"]["nodes"]);
}

#[test]
fn graph_db_evidence_packet_covers_backlog_job_worker_context_and_source_handles() {
    let project = graph_db_project();

    let backlog = graph_db_json(
        project.path(),
        Backend::Sqlite,
        vec![
            "evidence".to_string(),
            "gval".to_string(),
            "--depth".to_string(),
            "3".to_string(),
            "--limit".to_string(),
            "8".to_string(),
        ],
    );
    assert_eq!(backlog["target_node"]["kind"], "backlog", "{backlog}");
    assert_eq!(
        backlog["contract_version"], "graph-db-evidence-v1",
        "{backlog}"
    );
    assert!(
        backlog["packet_id"].as_str().unwrap().starts_with("gevd-"),
        "{backlog}"
    );
    assert!(backlog["projection_hash"].as_str().is_some(), "{backlog}");
    assert!(
        !backlog["worker_context"].as_array().unwrap().is_empty(),
        "{backlog}"
    );
    assert!(
        !backlog["source_handles"].as_array().unwrap().is_empty(),
        "{backlog}"
    );
    assert!(
        backlog["worker_results"].as_array().unwrap().iter().any(
            |node| node["properties"]["status"] == "completed"
                && node["properties"]["touched_files"] == "main.rs"
                && node["properties"]["follow_up_ids"] == "solo"
        ),
        "{backlog}"
    );
    assert!(
        backlog["shortest_paths"]
            .as_array()
            .unwrap()
            .iter()
            .any(|path| path["kind"] == "source_handle" && !path["path"].is_null()),
        "{backlog}"
    );
    assert!(
        backlog["next_commands"]
            .as_array()
            .unwrap()
            .iter()
            .any(|command| command.as_str().unwrap().contains("source-read")),
        "{backlog}"
    );
    assert!(
        backlog["replay_commands"]
            .as_array()
            .unwrap()
            .iter()
            .any(|command| command.as_str().unwrap().contains("evidence")),
        "{backlog}"
    );
    assert!(
        backlog["repair_commands"]
            .as_array()
            .unwrap()
            .iter()
            .any(|command| command.as_str().unwrap().contains("refresh")),
        "{backlog}"
    );
    assert!(
        backlog["fixture_coverage"]["test"]
            .as_str()
            .unwrap()
            .contains("graph_db_evidence_packet"),
        "{backlog}"
    );

    let job = graph_db_json(
        project.path(),
        Backend::Sqlite,
        vec![
            "kind".to_string(),
            "job_packet".to_string(),
            "--property".to_string(),
            "ref_id=gval".to_string(),
            "--limit".to_string(),
            "1".to_string(),
        ],
    );
    let job_id = node_ids(&job).remove(0);
    let job_evidence = graph_db_json(
        project.path(),
        Backend::Sqlite,
        vec![
            "evidence".to_string(),
            job_id,
            "--depth".to_string(),
            "3".to_string(),
        ],
    );
    assert_eq!(job_evidence["target_node"]["kind"], "job_packet");
    assert!(
        !job_evidence["worker_context"]
            .as_array()
            .unwrap()
            .is_empty(),
        "{job_evidence}"
    );
}

#[test]
fn conflict_matrix_cli_composes_planner_evidence_and_worker_ownership() {
    let project = graph_db_project();
    init_git_repo(project.path());
    let session = project.path().join("tasks/software/tsift.md");

    let report = assert_tsift_json(vec![
        "conflict-matrix".to_string(),
        "--path".to_string(),
        session.to_string_lossy().to_string(),
        "--json".to_string(),
        "gval".to_string(),
    ]);

    assert_eq!(report["targets"], json!(["gval"]));
    assert_eq!(report["contract_version"], "conflict-matrix-v1", "{report}");
    assert_eq!(report["cached_diff"]["mode"], "cached");
    assert_eq!(report["impact"]["mode"], "cached");
    assert!(
        report["inputs"]["cached_diff_command"]
            .as_str()
            .unwrap()
            .contains("diff-digest --cached"),
        "{report}"
    );
    assert!(
        report["inputs"]["evidence_packets"]
            .as_array()
            .unwrap()
            .iter()
            .any(
                |packet| packet["packet_id"].as_str().unwrap().starts_with("gevd-")
                    && packet["projection_hash"].as_str().is_some()
            ),
        "{report}"
    );
    let candidates = report["candidates"].as_array().unwrap();
    assert_eq!(candidates.len(), 1, "{report}");
    assert!(
        candidates[0]["evidence_packet_id"]
            .as_str()
            .unwrap()
            .starts_with("gevd-"),
        "{report}"
    );
    assert!(
        !candidates[0]["source_handles"]
            .as_array()
            .unwrap()
            .is_empty(),
        "{report}"
    );
    assert!(
        candidates[0]["ownership"]["prompt"]
            .as_str()
            .unwrap()
            .contains("Owned files"),
        "{report}"
    );
    assert!(
        report["next_commands"]
            .as_array()
            .unwrap()
            .iter()
            .any(|command| command.as_str().unwrap().contains("graph-db --path")),
        "{report}"
    );
}

#[test]
fn conflict_matrix_multi_worker_fixture_blocks_shared_files_and_emits_prompt_packets() {
    let project = graph_db_project();
    init_git_repo(project.path());
    let session = project.path().join("tasks/software/tsift.md");

    let evidence = graph_db_json(
        project.path(),
        Backend::Sqlite,
        vec![
            "evidence".to_string(),
            "solo".to_string(),
            "--depth".to_string(),
            "3".to_string(),
            "--limit".to_string(),
            "8".to_string(),
        ],
    );
    assert!(
        evidence["source_handles"]
            .as_array()
            .unwrap()
            .iter()
            .any(|node| node["properties"]["file"] == "isolated.rs"),
        "{evidence}"
    );

    let report = assert_tsift_json(vec![
        "conflict-matrix".to_string(),
        "--path".to_string(),
        session.to_string_lossy().to_string(),
        "--json".to_string(),
        "gval".to_string(),
        "shrd".to_string(),
        "solo".to_string(),
    ]);
    let candidates = report["candidates"].as_array().unwrap();
    assert_eq!(candidates.len(), 3, "{report}");

    let solo = candidates
        .iter()
        .find(|candidate| candidate["target"] == "solo")
        .unwrap();
    assert!(
        solo["owned_files"]
            .as_array()
            .unwrap()
            .iter()
            .any(|file| file == "isolated.rs"),
        "{report}"
    );
    assert!(
        solo["ownership"]["forbidden_files"]
            .as_array()
            .unwrap()
            .iter()
            .any(|file| file == "main.rs"),
        "{report}"
    );

    let pairs = report["conflicts"].as_array().unwrap();
    assert!(
        pairs.iter().any(|pair| {
            pair["risk"] == "fail_closed"
                && pair["shared_files"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|file| file == "main.rs")
        }),
        "{report}"
    );
    assert!(
        pairs.iter().any(|pair| {
            let left = pair["left"].as_str().unwrap();
            let right = pair["right"].as_str().unwrap();
            (left == "solo" || right == "solo")
                && pair["shared_files"].as_array().unwrap().is_empty()
        }),
        "{report}"
    );

    let packets = report["worker_prompt_packets"].as_array().unwrap();
    assert_eq!(packets.len(), 3, "{report}");
    let solo_packet = packets
        .iter()
        .find(|packet| packet["target"] == "solo")
        .unwrap();
    assert_eq!(
        solo_packet["contract_version"], "worker-prompt-packet-v1",
        "{report}"
    );
    assert!(
        solo_packet["packet_id"]
            .as_str()
            .unwrap()
            .starts_with("wpp-"),
        "{report}"
    );
    assert!(
        solo_packet["prompt"]
            .as_str()
            .unwrap()
            .contains("Expansion commands"),
        "{report}"
    );
    assert!(
        solo_packet["token_budget"]["source_window_count"]
            .as_u64()
            .unwrap()
            >= 1,
        "{report}"
    );
    assert!(
        report["orchestration"]["evidence_packet_ids"]
            .as_array()
            .unwrap()
            .iter()
            .all(|packet| packet.as_str().unwrap().starts_with("gevd-")),
        "{report}"
    );
    assert!(
        report["orchestration"]["conflict_matrix_decisions"]
            .as_array()
            .unwrap()
            .iter()
            .any(|decision| decision.as_str().unwrap().contains("candidate #")),
        "{report}"
    );
}

#[test]
fn conflict_matrix_worker_feedback_warns_on_repeated_blockage_without_changing_hard_gates() {
    let project = graph_db_project();
    init_git_repo(project.path());
    let session = project.path().join("tasks/software/tsift.md");

    let report = assert_tsift_json(vec![
        "conflict-matrix".to_string(),
        "--path".to_string(),
        session.to_string_lossy().to_string(),
        "--json".to_string(),
        "shrd".to_string(),
    ]);

    let candidate = report["candidates"]
        .as_array()
        .unwrap()
        .iter()
        .find(|candidate| candidate["target"] == "shrd")
        .unwrap();
    assert_eq!(candidate["worker_feedback"]["blocked"], 2, "{report}");
    assert_eq!(
        candidate["worker_feedback"]["repeated_blockage"], true,
        "{report}"
    );
    assert!(
        candidate["worker_feedback"]["touched_files"]
            .as_array()
            .unwrap()
            .iter()
            .any(|file| file == "main.rs"),
        "{report}"
    );
    assert!(
        candidate["worker_feedback"]["expected_tests"]
            .as_array()
            .unwrap()
            .iter()
            .any(|test| test == "cargo test --test graph_db_conformance"),
        "{report}"
    );
    assert!(
        candidate["worker_feedback"]["follow_up_ids"]
            .as_array()
            .unwrap()
            .iter()
            .any(|id| id == "gval"),
        "{report}"
    );
    assert!(
        candidate["worker_feedback"]["stale_expected_tests"]
            .as_array()
            .unwrap()
            .iter()
            .any(|test| test == "cargo test --test graph_db_conformance"),
        "{report}"
    );
    assert!(
        candidate["worker_feedback"]["follow_up_debt"]
            .as_array()
            .unwrap()
            .iter()
            .any(|id| id == "solo"),
        "{report}"
    );
    assert!(
        candidate["worker_feedback"]["closure_rank_score"]
            .as_u64()
            .unwrap()
            > 0,
        "{report}"
    );
    assert!(
        candidate["worker_feedback"]["closure_rank_reasons"]
            .as_array()
            .unwrap()
            .iter()
            .any(|reason| reason.as_str().unwrap().contains("stale expected tests")),
        "{report}"
    );
    assert!(
        report["warnings"]
            .as_array()
            .unwrap()
            .iter()
            .any(|warning| warning.as_str().unwrap().contains("repeated blockage")),
        "{report}"
    );
    assert!(
        report["warnings"]
            .as_array()
            .unwrap()
            .iter()
            .any(|warning| warning.as_str().unwrap().contains("follow-up debt")),
        "{report}"
    );
    assert!(
        candidate["risk_reasons"]
            .as_array()
            .unwrap()
            .iter()
            .all(|reason| !reason.as_str().unwrap().contains("repeated blockage")),
        "worker feedback should warn without weakening hard conflict gates: {report}"
    );
}

#[test]
fn conflict_matrix_worker_feedback_closure_score_reorders_safe_candidates() {
    let project = graph_db_project();
    init_git_repo(project.path());
    let session = project.path().join("tasks/software/tsift.md");

    let report = assert_tsift_json(vec![
        "conflict-matrix".to_string(),
        "--path".to_string(),
        session.to_string_lossy().to_string(),
        "--json".to_string(),
        "wfok".to_string(),
        "wfdb".to_string(),
    ]);

    let candidates = report["candidates"].as_array().unwrap();
    assert_eq!(candidates.len(), 2, "{report}");
    assert_eq!(candidates[0]["target"], "wfdb", "{report}");
    assert_eq!(candidates[0]["risk"], "low", "{report}");
    assert_eq!(candidates[1]["risk"], "low", "{report}");
    assert!(
        candidates[0]["worker_feedback"]["closure_rank_score"]
            .as_u64()
            .unwrap()
            > candidates[1]["worker_feedback"]["closure_rank_score"]
                .as_u64()
                .unwrap(),
        "{report}"
    );
    assert!(
        candidates[0]["worker_feedback"]["stale_expected_tests"]
            .as_array()
            .unwrap()
            .iter()
            .any(|test| test == "cargo test --test retired_closure"),
        "{report}"
    );
    assert!(
        candidates[0]["worker_feedback"]["follow_up_debt"]
            .as_array()
            .unwrap()
            .iter()
            .any(|id| id == "shrd"),
        "{report}"
    );
    assert!(
        candidates[0]["ownership"]["read_only_context"]
            .as_array()
            .unwrap()
            .iter()
            .any(|entry| entry.as_str().unwrap().contains("worker_feedback_closure")),
        "{report}"
    );
    assert!(
        candidates[0]["risk_reasons"]
            .as_array()
            .unwrap()
            .iter()
            .all(|reason| {
                let reason = reason.as_str().unwrap();
                !reason.contains("stale expected tests") && !reason.contains("follow-up debt")
            }),
        "{report}"
    );
}

#[test]
fn graph_orchestration_contract_fixture_matches_live_reports() {
    let project = graph_db_project();
    init_git_repo(project.path());
    let session = project.path().join("tasks/software/tsift.md");
    let fixture = graph_orchestration_contract_fixture();

    let evidence = graph_db_json(
        project.path(),
        Backend::Sqlite,
        vec![
            "evidence".to_string(),
            "gval".to_string(),
            "--depth".to_string(),
            "3".to_string(),
            "--limit".to_string(),
            "8".to_string(),
        ],
    );
    assert_contract_fields(&fixture, "graph_db_evidence", &evidence);
    assert!(evidence["packet_id"].as_str().unwrap().starts_with("gevd-"));
    assert!(evidence["projection_hash"].as_str().is_some());

    let conflict = assert_tsift_json(vec![
        "conflict-matrix".to_string(),
        "--path".to_string(),
        session.to_string_lossy().to_string(),
        "--json".to_string(),
        "gval".to_string(),
        "solo".to_string(),
    ]);
    assert_contract_fields(&fixture, "conflict_matrix", &conflict);
    for packet in conflict["worker_prompt_packets"].as_array().unwrap() {
        assert_contract_fields(&fixture, "worker_prompt_packet", packet);
    }

    let context_pack = assert_tsift_json(vec![
        "context-pack".to_string(),
        session.to_string_lossy().to_string(),
        "--budget".to_string(),
        "normal".to_string(),
        "--json".to_string(),
    ]);
    assert_contract_fields(
        &fixture,
        "context_pack_graph_orchestration",
        &context_pack["graph_orchestration"],
    );

    let session_review = assert_tsift_json(vec![
        "session-review".to_string(),
        session.to_string_lossy().to_string(),
        "--next-context".to_string(),
        "--budget".to_string(),
        "normal".to_string(),
        "--json".to_string(),
    ]);
    assert_contract_fields(&fixture, "session_review_follow_up", &session_review);

    let dispatch_trace = assert_tsift_json(vec![
        "dispatch-trace".to_string(),
        "--path".to_string(),
        session.to_string_lossy().to_string(),
        "--json".to_string(),
        "gval".to_string(),
        "solo".to_string(),
    ]);
    assert_contract_fields(&fixture, "dispatch_trace", &dispatch_trace);
    assert!(
        dispatch_trace["nodes"]
            .as_array()
            .unwrap()
            .iter()
            .any(|node| node["kind"] == "job_packet"),
        "{dispatch_trace}"
    );
    assert!(
        dispatch_trace["nodes"]
            .as_array()
            .unwrap()
            .iter()
            .any(|node| node["kind"] == "worker_result"),
        "{dispatch_trace}"
    );
    assert!(
        dispatch_trace["nodes"]
            .as_array()
            .unwrap()
            .iter()
            .any(|node| node["kind"] == "source_handle"),
        "{dispatch_trace}"
    );
    assert!(
        dispatch_trace["worker_prompt_packets"]
            .as_array()
            .unwrap()
            .iter()
            .all(|packet| packet["packet_id"].as_str().unwrap().starts_with("wpp-")),
        "{dispatch_trace}"
    );
}

#[test]
fn agent_orchestration_acceptance_pack_fixture_matches_queue_contract_terms() {
    let fixture = agent_orchestration_acceptance_pack_fixture();
    assert_eq!(
        fixture["version"], "agent-orchestration-acceptance-pack-v1",
        "{fixture}"
    );
    assert!(
        fixture["command_sequence"]
            .as_array()
            .unwrap()
            .iter()
            .any(|command| command
                .as_str()
                .unwrap()
                .contains("graph-db --path . refresh")),
        "{fixture}"
    );
    assert!(
        fixture["command_sequence"]
            .as_array()
            .unwrap()
            .iter()
            .any(
                |command| command.as_str().unwrap().contains("dispatch-trace")
                    && command.as_str().unwrap().contains("--format html")
            ),
        "{fixture}"
    );
    assert!(
        fixture["sample_rows"]["job_packets"]
            .as_array()
            .unwrap()
            .iter()
            .any(|row| row["properties"]["ref_id"] == "wfdb"),
        "{fixture}"
    );
    assert!(
        fixture["sample_rows"]["worker_results"]
            .as_array()
            .unwrap()
            .iter()
            .any(|row| {
                row["properties"]["ref_id"] == "wfdb"
                    && row["properties"]["status"] == "blocked"
                    && row["properties"]["expected_tests"] == "cargo test --test retired_closure"
            }),
        "{fixture}"
    );
    assert!(
        fixture["expected_contracts"]["worker_feedback"]["hard_gate_rule"]
            .as_str()
            .unwrap()
            .contains("must not add file/symbol/test/config risk reasons"),
        "{fixture}"
    );
    assert!(
        fixture["required_trace_links"]
            .as_array()
            .unwrap()
            .iter()
            .any(|link| link.as_str().unwrap().contains("replay_commands")),
        "{fixture}"
    );
}

#[test]
fn dispatch_trace_cli_exports_json_and_html_operator_views() {
    let project = graph_db_project();
    init_git_repo(project.path());
    let session = project.path().join("tasks/software/tsift.md");

    let report = assert_tsift_json(vec![
        "dispatch-trace".to_string(),
        "--path".to_string(),
        session.to_string_lossy().to_string(),
        "--json".to_string(),
        "gval".to_string(),
        "shrd".to_string(),
    ]);
    assert_eq!(report["contract_version"], "dispatch-trace-v1", "{report}");
    assert!(
        report["evidence_packet_ids"]
            .as_array()
            .unwrap()
            .iter()
            .all(|packet| packet.as_str().unwrap().starts_with("gevd-")),
        "{report}"
    );
    assert!(
        report["worker_feedback"]
            .as_array()
            .unwrap()
            .iter()
            .any(|feedback| feedback["repeated_blockage"] == true),
        "{report}"
    );

    let output = run_tsift(vec![
        "dispatch-trace".to_string(),
        "--path".to_string(),
        session.to_string_lossy().to_string(),
        "--format".to_string(),
        "html".to_string(),
        "gval".to_string(),
    ]);
    assert!(
        output.status.success(),
        "dispatch-trace html failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let html = String::from_utf8_lossy(&output.stdout);
    assert!(html.contains("id=\"graph-canvas\""), "{html}");
    assert!(html.contains("worker_prompt_packets"), "{html}");
    assert!(html.contains("dispatch-trace-v1"), "{html}");
}

#[test]
fn dispatch_trace_replay_contract_matches_real_queue_graph_db_run() {
    let project = graph_db_project();
    init_git_repo(project.path());
    let session = project.path().join("tasks/software/tsift.md");

    let refresh = graph_db_json(project.path(), Backend::Sqlite, vec!["refresh".to_string()]);
    assert_eq!(refresh["status"], "current", "{refresh}");

    let evidence = graph_db_json(
        project.path(),
        Backend::Sqlite,
        vec![
            "evidence".to_string(),
            "wfdb".to_string(),
            "--depth".to_string(),
            "3".to_string(),
            "--limit".to_string(),
            "8".to_string(),
        ],
    );
    let evidence_packet = evidence["packet_id"].as_str().unwrap().to_string();
    assert!(
        evidence["repair_commands"]
            .as_array()
            .unwrap()
            .iter()
            .any(|command| command.as_str().unwrap().contains("doctor")),
        "{evidence}"
    );

    let conflict = assert_tsift_json(vec![
        "conflict-matrix".to_string(),
        "--path".to_string(),
        session.to_string_lossy().to_string(),
        "--json".to_string(),
        "wfdb".to_string(),
        "wfok".to_string(),
    ]);
    assert!(
        conflict["worker_prompt_packets"]
            .as_array()
            .unwrap()
            .iter()
            .any(|packet| {
                packet["target"] == "wfdb"
                    && packet["worker_feedback"]["closure_rank_score"]
                        .as_u64()
                        .unwrap()
                        > 0
            }),
        "{conflict}"
    );

    let trace = assert_tsift_json(vec![
        "dispatch-trace".to_string(),
        "--path".to_string(),
        session.to_string_lossy().to_string(),
        "--json".to_string(),
        "wfdb".to_string(),
        "wfok".to_string(),
    ]);
    assert_eq!(
        trace["evidence_packet_ids"], conflict["orchestration"]["evidence_packet_ids"],
        "{trace}"
    );
    assert!(
        trace["evidence_packet_ids"]
            .as_array()
            .unwrap()
            .iter()
            .any(|packet| packet.as_str() == Some(evidence_packet.as_str())),
        "{trace}"
    );
    assert_eq!(
        trace["replay_commands"], conflict["next_commands"],
        "{trace}"
    );
    assert!(
        trace["repair_commands"]
            .as_array()
            .unwrap()
            .iter()
            .any(|command| command.as_str().unwrap().contains("refresh")),
        "{trace}"
    );

    let worker_rows = graph_db_json(
        project.path(),
        Backend::Sqlite,
        vec![
            "kind".to_string(),
            "worker_result".to_string(),
            "--property".to_string(),
            "ref_id=wfdb".to_string(),
            "--limit".to_string(),
            "5".to_string(),
        ],
    );
    let worker_ids = node_ids(&worker_rows);
    assert!(!worker_ids.is_empty(), "{worker_rows}");
    assert!(
        worker_ids.iter().all(|id| trace["nodes"]
            .as_array()
            .unwrap()
            .iter()
            .any(|node| node["id"].as_str() == Some(id.as_str()))),
        "{trace}"
    );
    assert!(
        trace["nodes"]
            .as_array()
            .unwrap()
            .iter()
            .any(|node| { node["kind"] == "job_packet" && node["properties"]["ref_id"] == "wfdb" }),
        "{trace}"
    );
    assert!(
        trace["nodes"]
            .as_array()
            .unwrap()
            .iter()
            .any(|node| node["kind"] == "source_handle"),
        "{trace}"
    );
    assert!(
        trace["worker_feedback"]
            .as_array()
            .unwrap()
            .iter()
            .any(|feedback| {
                feedback["stale_expected_tests"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|test| test == "cargo test --test retired_closure")
                    && feedback["follow_up_debt"]
                        .as_array()
                        .unwrap()
                        .iter()
                        .any(|id| id == "shrd")
            }),
        "{trace}"
    );

    let output = run_tsift(vec![
        "dispatch-trace".to_string(),
        "--path".to_string(),
        session.to_string_lossy().to_string(),
        "--format".to_string(),
        "html".to_string(),
        "wfdb".to_string(),
    ]);
    assert!(
        output.status.success(),
        "dispatch-trace html failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let html = String::from_utf8_lossy(&output.stdout);
    assert!(html.contains(&evidence_packet), "{html}");
    assert!(html.contains("Follow-up debt"), "{html}");
    assert!(html.contains("closure"), "{html}");
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
fn graph_db_drift_report_summarizes_snapshot_diff_and_failures() {
    let project = graph_db_project();
    let mut snapshot = current_convex_snapshot(project.path());
    attach_required_convex_indexes(&mut snapshot);

    let nodes = snapshot["nodes"].as_array_mut().unwrap();
    let removed = nodes
        .iter()
        .position(|node| node["kind"] == "backlog")
        .unwrap();
    nodes.remove(removed);
    let duplicate_node = nodes
        .iter()
        .find(|node| node["kind"] == "symbol")
        .unwrap()
        .clone();
    nodes.push(duplicate_node);
    let stale = nodes
        .iter_mut()
        .find(|node| node["kind"] == "projection_meta")
        .unwrap();
    stale["properties"]["content_hash"] = json!("stale-snapshot-hash");
    nodes.push(json!({
        "externalId": "stale-remote-node",
        "kind": "backlog",
        "label": "stale",
        "properties": {},
        "provenance": []
    }));
    let removed_edge = snapshot["edges"]
        .as_array()
        .unwrap()
        .iter()
        .position(|edge| edge["kind"] == "calls")
        .unwrap();
    snapshot["edges"]
        .as_array_mut()
        .unwrap()
        .remove(removed_edge);
    snapshot["edges"].as_array_mut().unwrap().push(json!({
        "edgeKey": "stale-remote-edge",
        "fromExternalId": "stale-remote-node",
        "toExternalId": "stale-remote-node",
        "kind": "mentions",
        "properties": {},
        "provenance": []
    }));
    snapshot["edges"].as_array_mut().unwrap().push(json!({
        "edgeKey": "edge:orphan",
        "fromExternalId": "missing-symbol",
        "toExternalId": "stale-remote-node",
        "kind": "calls",
        "properties": {},
        "provenance": []
    }));
    let snapshot_path = write_snapshot(project.path(), "convex-drift-bad.json", &snapshot);

    let report = assert_tsift_json(vec![
        "graph-db".to_string(),
        "--path".to_string(),
        project.path().to_string_lossy().to_string(),
        "--backend".to_string(),
        "convex-snapshot".to_string(),
        "--convex-snapshot".to_string(),
        snapshot_path.to_string_lossy().to_string(),
        "--json".to_string(),
        "drift".to_string(),
    ]);

    assert_eq!(report["status"], "fail_closed", "{report}");
    assert!(report["summary"]["node_upserts"].as_u64().unwrap() > 0);
    assert!(report["summary"]["edge_upserts"].as_u64().unwrap() > 0);
    assert_eq!(report["summary"]["node_tombstones"], 1);
    assert!(report["summary"]["edge_tombstones"].as_u64().unwrap() >= 1);
    assert!(
        report["summary"]["stale_projection_metadata"]
            .as_u64()
            .unwrap()
            > 0
    );
    assert!(report["summary"]["duplicate_failures"].as_u64().unwrap() > 0);
    assert!(report["summary"]["orphan_failures"].as_u64().unwrap() > 0);
    assert!(
        report["next_commands"]
            .as_array()
            .unwrap()
            .iter()
            .any(|command| command.as_str().unwrap().contains("convex-sync")),
        "{report}"
    );
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
            .any(|command| command.as_str().unwrap().contains("graph-db")
                && command.as_str().unwrap().contains("refresh")),
        "{report}"
    );
    assert!(stderr.contains("graph-db doctor failed closed"), "{stderr}");
}

fn large_graph_db_project(symbol_count: usize) -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    let mut source = String::new();
    source.push_str("fn main() { f000(); }\n");
    for idx in 0..symbol_count {
        if idx + 1 < symbol_count {
            source.push_str(&format!("fn f{idx:03}() {{ f{:03}(); }}\n", idx + 1));
        } else {
            source.push_str(&format!("fn f{idx:03}() {{}}\n"));
        }
    }
    fs::write(dir.path().join("main.rs"), source).unwrap();

    let task_dir = dir.path().join("tasks/software");
    fs::create_dir_all(&task_dir).unwrap();
    let mut backlog = String::from(
        r#"---
agent_doc_session: tsift-scale
agent_doc_format: template
---

## Backlog

<!-- agent:backlog -->
"#,
    );
    for idx in 0..30 {
        backlog.push_str(&format!(
            "- [ ] [#b{idx:03}] Trace f{idx:03} graph-db scale coverage through calls and pagination.\n"
        ));
    }
    backlog.push_str("<!-- /agent:backlog -->\n");
    fs::write(task_dir.join("tsift.md"), backlog).unwrap();

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

fn collect_paged_symbol_ids(project: &Path, limit: usize) -> Vec<String> {
    let mut ids = Vec::new();
    let mut cursor = None;
    loop {
        let mut query = vec![
            "kind".to_string(),
            "symbol".to_string(),
            "--property".to_string(),
            "path=main.rs".to_string(),
            "--limit".to_string(),
            limit.to_string(),
        ];
        if let Some(cursor) = cursor.take() {
            query.push("--cursor".to_string());
            query.push(cursor);
        }
        let page = graph_db_json(project, Backend::Sqlite, query);
        ids.extend(node_ids(&page));
        cursor = page["page"]["next_cursor"].as_str().map(str::to_string);
        if cursor.is_none() {
            break;
        }
    }
    ids
}

fn query_plan_details(db_path: &Path, sql: &str) -> Vec<String> {
    let conn = Connection::open(db_path).unwrap();
    let mut stmt = conn.prepare(&format!("EXPLAIN QUERY PLAN {sql}")).unwrap();
    stmt.query_map([], |row| row.get::<_, String>(3))
        .unwrap()
        .collect::<Result<Vec<_>, _>>()
        .unwrap()
}

#[test]
fn graph_db_scale_caps_pagination_paths_doctor_and_sqlite_plans() {
    let project = large_graph_db_project(120);
    let first_id = symbol_id_by_ref(project.path(), Backend::Sqlite, "f000");
    let far_id = symbol_id_by_ref(project.path(), Backend::Sqlite, "f080");

    let paged_ids = collect_paged_symbol_ids(project.path(), 17);
    assert!(
        paged_ids.len() >= 120,
        "expected all symbols: {paged_ids:?}"
    );
    assert_sorted(&paged_ids);
    let mut deduped = paged_ids.clone();
    deduped.dedup();
    assert_eq!(deduped.len(), paged_ids.len(), "pagination duplicated ids");

    let selective = graph_db_json(
        project.path(),
        Backend::Sqlite,
        vec![
            "kind".to_string(),
            "symbol".to_string(),
            "--property".to_string(),
            "ref_id=f042".to_string(),
            "--limit".to_string(),
            "10".to_string(),
        ],
    );
    assert_eq!(node_ids(&selective).len(), 1, "{selective}");

    let neighborhood = graph_db_json(
        project.path(),
        Backend::Sqlite,
        vec![
            "neighborhood".to_string(),
            first_id.clone(),
            "--depth".to_string(),
            "200".to_string(),
            "--edge-kind".to_string(),
            "calls".to_string(),
            "--limit".to_string(),
            "13".to_string(),
        ],
    );
    assert_eq!(neighborhood["page"]["returned_nodes"], 13);
    assert!(neighborhood["page"]["truncated"].as_bool().unwrap());
    assert!(neighborhood["edges"].as_array().unwrap().len() <= 12);

    let capped_path = graph_db_json(
        project.path(),
        Backend::Sqlite,
        vec![
            "path".to_string(),
            first_id.clone(),
            far_id.clone(),
            "--edge-kind".to_string(),
            "calls".to_string(),
            "--max-hops".to_string(),
            "20".to_string(),
        ],
    );
    assert!(capped_path["path"].is_null(), "{capped_path}");
    assert!(
        capped_path["warnings"]
            .as_array()
            .unwrap()
            .iter()
            .any(|warning| warning.as_str().unwrap().contains("--max-hops 20")),
        "{capped_path}"
    );

    let full_path = graph_db_json(
        project.path(),
        Backend::Sqlite,
        vec![
            "path".to_string(),
            first_id.clone(),
            far_id,
            "--edge-kind".to_string(),
            "calls".to_string(),
            "--max-hops".to_string(),
            "200".to_string(),
        ],
    );
    assert_eq!(full_path["path"]["hops"], 80, "{full_path}");

    let started = Instant::now();
    let doctor = graph_db_json(project.path(), Backend::Sqlite, vec!["doctor".to_string()]);
    assert_eq!(doctor["status"], "ok", "{doctor}");
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "doctor should stay bounded on synthetic graph"
    );

    let db_path = graph_db_path(project.path());
    let node_plan = query_plan_details(
        &db_path,
        "SELECT id FROM graph_nodes WHERE kind = 'symbol' ORDER BY id",
    )
    .join("\n");
    assert!(
        node_plan.contains("idx_graph_nodes_kind"),
        "expected graph_nodes kind index in plan:\n{node_plan}"
    );
    let edge_plan = query_plan_details(
        &db_path,
        &format!(
            "SELECT to_id FROM graph_edges INDEXED BY idx_graph_edges_from_kind WHERE from_id = '{}' AND kind = 'calls' ORDER BY to_id, kind",
            first_id.replace('\'', "''")
        ),
    )
    .join("\n");
    assert!(
        edge_plan.contains("idx_graph_edges_from_kind"),
        "expected graph_edges from/kind index in plan:\n{edge_plan}"
    );
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
