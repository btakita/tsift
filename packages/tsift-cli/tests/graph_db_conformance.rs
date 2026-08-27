use rusqlite::Connection;
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet};
use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::time::{Duration, Instant};

const LIVE_CONVEX_ACCEPTANCE_ENV: &str = "TSIFT_LIVE_CONVEX_ACCEPTANCE";
const LIVE_CONVEX_GRAPH_URL_ENV: &str = "TSIFT_LIVE_CONVEX_GRAPH_URL";
const LIVE_CONVEX_AUTH_TOKEN_ENV: &str = "TSIFT_LIVE_CONVEX_AUTH_TOKEN";

fn tsift_bin() -> Command {
    Command::new(env!("CARGO_BIN_EXE_tsift-cli"))
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

fn seed_current_semantic_summary(project: &Path, file_path: &str, symbol_name: &str) {
    fs::create_dir_all(project.join(".tsift")).unwrap();
    let content = fs::read(project.join(file_path)).unwrap();
    let content_hash = blake3::hash(&content).to_hex().to_string();
    let conn = Connection::open(project.join(".tsift/summaries.db")).unwrap();
    conn.execute_batch(
        "PRAGMA journal_mode=WAL;
         CREATE TABLE IF NOT EXISTS summaries (
             id INTEGER PRIMARY KEY,
             symbol_name TEXT NOT NULL,
             file_path TEXT NOT NULL,
             content_hash TEXT NOT NULL,
             summary TEXT NOT NULL,
             entities TEXT,
             relationships TEXT,
             concept_labels TEXT,
             extracted_at TEXT NOT NULL,
             model TEXT NOT NULL,
             tokens_input INTEGER,
             tokens_output INTEGER
         );
         CREATE INDEX IF NOT EXISTS idx_summaries_symbol ON summaries(symbol_name);
         CREATE INDEX IF NOT EXISTS idx_summaries_file ON summaries(file_path);
         CREATE INDEX IF NOT EXISTS idx_summaries_hash ON summaries(content_hash);",
    )
    .unwrap();
    let entities = serde_json::to_string(&json!([
        {
            "name": symbol_name,
            "kind": "function",
            "description": "Builds graph navigation evidence."
        },
        {
            "name": "TraversalGraph",
            "kind": "type",
            "description": "Carries GraphStore-backed traversal rows."
        }
    ]))
    .unwrap();
    let relationships = serde_json::to_string(&json!([
        {
            "from": symbol_name,
            "to": "TraversalGraph",
            "kind": "uses"
        }
    ]))
    .unwrap();
    let concept_labels =
        serde_json::to_string(&json!(["graph navigation", "semantic extraction"])).unwrap();
    conn.execute(
        "INSERT INTO summaries (
            symbol_name,
            file_path,
            content_hash,
            summary,
            entities,
            relationships,
            concept_labels,
            extracted_at,
            model,
            tokens_input,
            tokens_output
        ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
        rusqlite::params![
            symbol_name,
            file_path,
            content_hash,
            "cached summary links helper to graph navigation evidence",
            entities,
            relationships,
            concept_labels,
            "1700000000",
            "test-model",
            100_i64,
            40_i64
        ],
    )
    .unwrap();
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

fn seed_conflict_matrix_cache_index_artifact(path: &Path) -> String {
    let artifact = ".tsift/conflict-matrix-cache/generated.rs";
    let artifact_path = path.join(artifact);
    fs::create_dir_all(artifact_path.parent().unwrap()).unwrap();
    fs::write(&artifact_path, "fn cache_artifact_symbol() {}\n").unwrap();

    let conn = Connection::open(path.join(".tsift/index.db")).unwrap();
    conn.execute(
        "INSERT OR REPLACE INTO file_state (path, mtime_secs, mtime_nanos, language)
         VALUES (?1, ?2, ?3, ?4)",
        rusqlite::params![artifact, 1_i64, 0_i64, "rust"],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO symbols (name, kind, language, signature, file, line, end_line, parent_module, visibility, tags)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, NULL, NULL, NULL)",
        rusqlite::params![
            "cache_artifact_symbol",
            "function",
            "rust",
            "fn cache_artifact_symbol()",
            artifact,
            1_i64,
            1_i64
        ],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO call_edges (caller_file, caller_name, caller_line, callee_name, call_site_line)
         VALUES (?1, ?2, ?3, ?4, ?5)",
        rusqlite::params![artifact, "cache_artifact_symbol", 1_i64, "helper", 1_i64],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO route_nodes (framework, method, route_path, handler_name, file, line, handler_line)
         VALUES (?1, NULL, ?2, ?3, ?4, ?5, ?6)",
        rusqlite::params![
            "test",
            "/cache-artifact",
            "cache_artifact_symbol",
            artifact,
            1_i64,
            1_i64
        ],
    )
    .unwrap();
    artifact.to_string()
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

#[test]
fn graph_db_accepts_trailing_json_and_fails_closed_for_missing_targets() {
    let project = graph_db_project();
    let root = project.path().to_string_lossy().to_string();
    let _refresh = assert_tsift_json(vec![
        "graph-db".to_string(),
        "--path".to_string(),
        root,
        "refresh".to_string(),
        "--json".to_string(),
    ]);
    for query in [
        vec!["node".to_string(), "gsym-does-not-exist".to_string()],
        vec!["edge".to_string(), "gedge-does-not-exist".to_string()],
        vec!["incident".to_string(), "gsym-does-not-exist".to_string()],
        vec![
            "neighborhood".to_string(),
            "gsym-does-not-exist".to_string(),
        ],
    ] {
        let error = graph_db_failure(project.path(), Backend::Sqlite, query);
        assert!(error.contains("not found"), "{error}");
    }

    let error = graph_db_failure(
        project.path(),
        Backend::Sqlite,
        vec!["kind".to_string(), "function".to_string()],
    );
    assert!(error.contains("available kinds:"), "{error}");
    assert!(error.contains("symbol"), "{error}");
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
    // Uses the paginated snapshot ops (#convexsnapshotscale). The single-shot
    // `snapshot` operation hits Convex's 15s syscall budget once tables exceed
    // ~5k rows.
    let post = |body: Value| -> Value {
        let mut builder = ureq::post(endpoint);
        if let Ok(token) = env::var(auth_token_env)
            && !token.trim().is_empty()
        {
            builder = builder.header("Authorization", &format!("Bearer {token}"));
        }
        let mut response = builder.send_json(&body).unwrap_or_else(|err| {
            panic!("live Convex snapshot request failed for {endpoint} body={body}: {err}")
        });
        let response: Value = response.body_mut().read_json().unwrap_or_else(|err| {
            panic!("live Convex snapshot response was not JSON for body={body}: {err}")
        });
        assert_eq!(
            response["status"], "ok",
            "unexpected snapshot response for body={body}: {response}"
        );
        response
    };

    let meta = post(json!({
        "operation": "snapshot_meta",
        "chunk": 0,
        "projectionVersion": projection_version,
    }));
    assert!(
        meta["meta"]["indexes"].is_array(),
        "meta missing required indexes: {meta}"
    );
    assert!(
        meta["meta"]["pageSize"].is_number(),
        "meta missing pageSize: {meta}"
    );

    let mut nodes: Vec<Value> = Vec::new();
    let mut cursor: Value = Value::Null;
    loop {
        let resp = post(json!({
            "operation": "snapshot_nodes_page",
            "chunk": 0,
            "projectionVersion": projection_version,
            "cursor": cursor,
        }));
        let rows = resp["page"]["rows"].as_array().cloned().unwrap_or_default();
        nodes.extend(rows);
        let next = resp["page"]["nextCursor"].clone();
        if next.is_null() {
            break;
        }
        cursor = next;
    }

    let mut edges: Vec<Value> = Vec::new();
    let mut cursor: Value = Value::Null;
    loop {
        let resp = post(json!({
            "operation": "snapshot_edges_page",
            "chunk": 0,
            "projectionVersion": projection_version,
            "cursor": cursor,
        }));
        let rows = resp["page"]["rows"].as_array().cloned().unwrap_or_default();
        edges.extend(rows);
        let next = resp["page"]["nextCursor"].clone();
        if next.is_null() {
            break;
        }
        cursor = next;
    }

    json!({ "nodes": nodes, "edges": edges })
}

fn node_ids(report: &Value) -> Vec<String> {
    report["nodes"]
        .as_array()
        .unwrap()
        .iter()
        .map(|node| node["id"].as_str().unwrap().to_string())
        .collect()
}

fn graph_db_nodes_empty(report: &Value) -> bool {
    report["node"].is_null()
        && report["nodes"]
            .as_array()
            .is_none_or(|nodes| nodes.is_empty())
}

fn phase_detail<'a>(report: &'a Value, phase: &str) -> &'a str {
    report["phase_timings"]
        .as_array()
        .unwrap()
        .iter()
        .find(|entry| entry["name"] == phase)
        .and_then(|entry| entry["detail"].as_str())
        .unwrap_or_else(|| panic!("missing phase {phase}: {report}"))
}

fn phase_duration(report: &Value, phase: &str) -> u64 {
    report["phase_timings"]
        .as_array()
        .unwrap()
        .iter()
        .find(|entry| entry["name"] == phase)
        .and_then(|entry| entry["duration_micros"].as_u64())
        .unwrap_or_else(|| panic!("missing phase {phase}: {report}"))
}

fn backend_eval_backend<'a>(dataset: &'a Value, backend: &str, report: &Value) -> &'a Value {
    dataset["backends"]
        .as_array()
        .unwrap()
        .iter()
        .find(|entry| entry["backend"] == backend)
        .unwrap_or_else(|| {
            panic!(
                "missing backend-eval backend {backend} in dataset {}: {report}",
                dataset["name"]
            )
        })
}

fn backend_eval_operation<'a>(backend: &'a Value, operation: &str, report: &Value) -> &'a Value {
    backend["operations"]
        .as_array()
        .unwrap()
        .iter()
        .find(|entry| entry["name"] == operation)
        .unwrap_or_else(|| {
            panic!(
                "missing backend-eval operation {operation} for backend {}: {report}",
                backend["backend"]
            )
        })
}

fn assert_backend_eval_metric(report: &Value, metric: &str) {
    assert!(
        report["metrics"].as_object().unwrap().contains_key(metric),
        "missing backend-eval metric {metric}: {report}"
    );
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

fn stale_convex_snapshot_fixture_path() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("fixtures/graph-db-operator-examples/stale-convex-snapshot.json")
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

fn string_values(value: &Value) -> Vec<String> {
    value
        .as_array()
        .unwrap()
        .iter()
        .map(|entry| entry.as_str().unwrap().to_string())
        .collect()
}

fn sorted_property_values(nodes: &Value, property: &str) -> Vec<String> {
    nodes
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|node| node["properties"][property].as_str())
        .collect::<BTreeSet<_>>()
        .into_iter()
        .map(str::to_string)
        .collect()
}

fn worker_result_summaries(nodes: &Value) -> Vec<Value> {
    let mut summaries = nodes
        .as_array()
        .unwrap()
        .iter()
        .map(|node| {
            let properties = &node["properties"];
            let ref_id = properties["ref_id"].as_str().unwrap_or("").to_string();
            let status = properties["status"].as_str().unwrap_or("").to_string();
            let summary = json!({
                "ref_id": ref_id,
                "status": status,
                "touched_files": properties.get("touched_files").cloned().unwrap_or(Value::Null),
                "expected_tests": properties.get("expected_tests").cloned().unwrap_or(Value::Null),
                "follow_up_ids": properties.get("follow_up_ids").cloned().unwrap_or(Value::Null),
            });
            (format!("{ref_id}:{status}"), summary)
        })
        .collect::<Vec<_>>();
    summaries.sort_by(|left, right| left.0.cmp(&right.0));
    summaries.into_iter().map(|(_, summary)| summary).collect()
}

fn worker_feedback_summary(feedback: &Value) -> Value {
    json!({
        "completed": feedback["completed"],
        "blocked": feedback["blocked"],
        "repeated_blockage": feedback["repeated_blockage"],
        "stale_expected_tests": feedback["stale_expected_tests"],
        "follow_up_debt": feedback["follow_up_debt"],
        "closure_rank_score": feedback["closure_rank_score"],
        "closure_rank_reasons": feedback["closure_rank_reasons"],
    })
}

fn conflict_candidate_summaries(report: &Value) -> Vec<Value> {
    report["candidates"]
        .as_array()
        .unwrap()
        .iter()
        .map(|candidate| {
            let read_only_context = string_values(&candidate["ownership"]["read_only_context"]);
            json!({
                "rank": candidate["rank"],
                "target": candidate["target"],
                "risk": candidate["risk"],
                "previously_completed": candidate["previously_completed"],
                "parallel_safe": candidate["parallel_safe"],
                "blocks": candidate["blocks"],
                "blocked_by": candidate["blocked_by"],
                "owned_files": candidate["owned_files"],
                "owned_symbols": candidate["owned_symbols"],
                "affected_tests": candidate["affected_tests"],
                "required_context_source_handles": candidate["required_context"]["source_handles"],
                "graph_handle_prefixes": {
                    "target_node": candidate["graph_handles"]["target_node_id"].as_str().unwrap_or("").split(':').next().unwrap_or(""),
                    "evidence_packet": candidate["graph_handles"]["evidence_packet_id"].as_str().unwrap_or("").split('-').next().unwrap_or(""),
                    "worker_prompt_packet": candidate["graph_handles"]["worker_prompt_packet_id"].as_str().unwrap_or("").split('-').next().unwrap_or(""),
                },
                "worker_feedback": worker_feedback_summary(&candidate["worker_feedback"]),
                "read_only_context_has_worker_feedback": read_only_context
                    .iter()
                    .any(|entry| entry.contains("worker_feedback:")),
                "read_only_context_has_closure_warning": read_only_context
                    .iter()
                    .any(|entry| entry.contains("worker_feedback_closure:")),
            })
        })
        .collect()
}

fn worker_prompt_packet_summaries(report: &Value) -> Vec<Value> {
    report["worker_prompt_packets"]
        .as_array()
        .unwrap()
        .iter()
        .map(|packet| {
            json!({
                "contract_version": packet["contract_version"],
                "target": packet["target"],
                "rank": packet["rank"],
                "risk": packet["risk"],
                "previously_completed": packet["previously_completed"],
                "parallel_safe": packet["parallel_safe"],
                "blocks": packet["blocks"],
                "blocked_by": packet["blocked_by"],
                "packet_id_prefix": packet["packet_id"]
                    .as_str()
                    .unwrap()
                    .split('-')
                    .next()
                    .unwrap(),
                "owned_files": packet["owned_files"],
                "required_context_source_handles": packet["required_context"]["source_handles"],
                "graph_handle_prefixes": {
                    "target_node": packet["graph_handles"]["target_node_id"].as_str().unwrap_or("").split(':').next().unwrap_or(""),
                    "evidence_packet": packet["graph_handles"]["evidence_packet_id"].as_str().unwrap_or("").split('-').next().unwrap_or(""),
                    "worker_prompt_packet": packet["graph_handles"]["worker_prompt_packet_id"].as_str().unwrap_or("").split('-').next().unwrap_or(""),
                },
                "worker_feedback": worker_feedback_summary(&packet["worker_feedback"]),
            })
        })
        .collect()
}

fn chunk_operations(report: &Value) -> Vec<String> {
    report["chunks"]
        .as_array()
        .unwrap()
        .iter()
        .map(|chunk| chunk["operation"].as_str().unwrap().to_string())
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect()
}

fn node_kind_counts(nodes: &Value) -> BTreeMap<String, usize> {
    let mut counts = BTreeMap::new();
    for node in nodes.as_array().unwrap() {
        let kind = node["kind"].as_str().unwrap().to_string();
        *counts.entry(kind).or_insert(0) += 1;
    }
    counts
}

fn commands_contain(commands: &Value, needle: &str) -> bool {
    commands
        .as_array()
        .unwrap()
        .iter()
        .any(|command| command.as_str().unwrap().contains(needle))
}

fn commands_contain_all(commands: &Value, needles: &[&str]) -> bool {
    commands.as_array().unwrap().iter().any(|command| {
        let command = command.as_str().unwrap();
        needles.iter().all(|needle| command.contains(needle))
    })
}

fn regenerate_agent_orchestration_acceptance_samples(project: &Path, session: &Path) -> Value {
    let refresh = graph_db_json(project, Backend::Sqlite, vec!["refresh".to_string()]);
    let status = graph_db_json(project, Backend::Sqlite, vec!["status".to_string()]);
    let doctor = graph_db_json(project, Backend::Sqlite, vec!["doctor".to_string()]);
    let evidence = graph_db_json(
        project,
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
    let stale_snapshot = stale_convex_snapshot_fixture_path();
    let drift = assert_tsift_json(vec![
        "graph-db".to_string(),
        "--path".to_string(),
        project.to_string_lossy().to_string(),
        "--backend".to_string(),
        "convex-snapshot".to_string(),
        "--convex-snapshot".to_string(),
        stale_snapshot.to_string_lossy().to_string(),
        "--json".to_string(),
        "drift".to_string(),
    ]);
    let stale_sync = assert_tsift_json(vec![
        "convex-sync".to_string(),
        project.to_string_lossy().to_string(),
        "--snapshot".to_string(),
        stale_snapshot.to_string_lossy().to_string(),
        "--chunk-size".to_string(),
        "25".to_string(),
        "--json".to_string(),
    ]);
    let (stale_doctor, _stderr) = assert_tsift_failure_json(graph_db_args(
        project,
        Backend::ConvexSnapshot(&stale_snapshot),
        vec!["doctor".to_string()],
    ));
    let conflict = assert_tsift_json(vec![
        "conflict-matrix".to_string(),
        "--path".to_string(),
        session.to_string_lossy().to_string(),
        "--json".to_string(),
        "wfdb".to_string(),
        "wfok".to_string(),
    ]);
    let dependency_dag = assert_tsift_json(vec![
        "dependency-dag".to_string(),
        "--path".to_string(),
        session.to_string_lossy().to_string(),
        "--json".to_string(),
        "wfdb".to_string(),
        "shrd".to_string(),
        "wfok".to_string(),
    ]);
    let dependency_dag_cycle = assert_tsift_json(vec![
        "dependency-dag".to_string(),
        "--path".to_string(),
        session.to_string_lossy().to_string(),
        "--json".to_string(),
        "gval".to_string(),
        "solo".to_string(),
    ]);
    let trace = assert_tsift_json(vec![
        "dispatch-trace".to_string(),
        "--path".to_string(),
        session.to_string_lossy().to_string(),
        "--json".to_string(),
        "wfdb".to_string(),
        "wfok".to_string(),
    ]);
    let html_output = run_tsift(vec![
        "dispatch-trace".to_string(),
        "--path".to_string(),
        session.to_string_lossy().to_string(),
        "--format".to_string(),
        "html".to_string(),
        "wfdb".to_string(),
    ]);
    assert!(
        html_output.status.success(),
        "dispatch-trace html failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&html_output.stdout),
        String::from_utf8_lossy(&html_output.stderr)
    );
    let html = String::from_utf8_lossy(&html_output.stdout);
    let context_pack = assert_tsift_json(vec![
        "context-pack".to_string(),
        session.to_string_lossy().to_string(),
        "--budget".to_string(),
        "normal".to_string(),
        "--json".to_string(),
    ]);
    let session_review = assert_tsift_json(vec![
        "session-review".to_string(),
        session.to_string_lossy().to_string(),
        "--next-context".to_string(),
        "--budget".to_string(),
        "normal".to_string(),
        "--json".to_string(),
    ]);

    let trace_feedback = trace["worker_feedback"]
        .as_array()
        .unwrap()
        .iter()
        .map(worker_feedback_summary)
        .collect::<Vec<_>>();
    let context_graph = &context_pack["graph_orchestration"];

    json!({
        "graph_db_refresh": {
            "status": refresh["status"],
            "projection_version": refresh["freshness"]["projection_version"],
            "content_hash_present": refresh["freshness"]["content_hash"].as_str().is_some(),
            "next_mentions_status": commands_contain(&refresh["next_commands"], "status"),
            "next_mentions_doctor": commands_contain(&refresh["next_commands"], "doctor"),
            "next_mentions_drift": commands_contain(&refresh["next_commands"], "drift"),
            "next_mentions_convex_sync": commands_contain(&refresh["next_commands"], "convex-sync"),
        },
        "graph_db_status": {
            "operation": status["operation"],
            "status": status["status"],
            "materialized": status["materialized"],
            "counts_match_refresh": status["counts"] == refresh["counts"],
            "next_mentions_doctor": commands_contain(&status["next_commands"], "doctor"),
            "next_mentions_drift": commands_contain(&status["next_commands"], "drift"),
            "next_mentions_convex_sync": commands_contain(&status["next_commands"], "convex-sync"),
        },
        "graph_db_doctor": {
            "status": doctor["status"],
            "backend": doctor["backend"],
            "fail_closed": doctor["fail_closed"],
            "check_count": doctor["checks"].as_array().unwrap().len(),
            "repair_command_count": doctor["repair_commands"].as_array().unwrap().len(),
        },
        "graph_db_evidence": {
            "contract_version": evidence["contract_version"],
            "target": evidence["target"],
            "packet_id_prefix": evidence["packet_id"].as_str().unwrap().split('-').next().unwrap(),
            "projection_hash_present": evidence["projection_hash"].as_str().is_some(),
            "freshness_status": evidence["freshness"]["status"],
            "target_kind": evidence["target_node"]["kind"],
            "target_ref_id": evidence["target_node"]["properties"]["ref_id"],
            "source_handle_files": sorted_property_values(&evidence["source_handles"], "file"),
            "worker_results": worker_result_summaries(&evidence["worker_results"]),
            "replay_mentions_wfdb": commands_contain_all(&evidence["replay_commands"], &["evidence", "wfdb"]),
            "repair_mentions_refresh": commands_contain(&evidence["repair_commands"], "refresh"),
            "repair_mentions_doctor": commands_contain(&evidence["repair_commands"], "doctor"),
        },
        "stale_convex_snapshot_drift": {
            "status": drift["status"],
            "graph_reads_allowed": drift["graph_reads_allowed"],
            "node_upserts_positive": drift["summary"]["node_upserts"].as_u64().unwrap() > 0,
            "edge_upserts_positive": drift["summary"]["edge_upserts"].as_u64().unwrap() > 0,
            "node_tombstones": drift["summary"]["node_tombstones"],
            "edge_tombstones": drift["summary"]["edge_tombstones"],
            "missing_required_indexes_positive": drift["summary"]["missing_required_indexes"].as_u64().unwrap() > 0,
            "stale_projection_metadata_positive": drift["summary"]["stale_projection_metadata"].as_u64().unwrap() > 0,
            "next_mentions_doctor": commands_contain(&drift["next_commands"], "doctor"),
            "next_mentions_convex_sync_snapshot": commands_contain_all(&drift["next_commands"], &["convex-sync", "--snapshot"]),
            "next_mentions_convex_apply": commands_contain_all(&drift["next_commands"], &["convex-sync", "--remote-snapshot", "--apply"]),
        },
        "stale_convex_snapshot_doctor": {
            "status": stale_doctor["status"],
            "backend": stale_doctor["backend"],
            "fail_closed": stale_doctor["fail_closed"],
            "required_index_count": stale_doctor["required_indexes"].as_array().unwrap().len(),
            "repair_mentions_convex_sync": commands_contain(&stale_doctor["repair_commands"], "convex-sync"),
            "repair_mentions_schema_example": commands_contain(&stale_doctor["repair_commands"], "examples/convex-graph/schema.ts"),
        },
        "stale_convex_snapshot_sync": {
            "dry_run": stale_sync["dry_run"],
            "freshness_status": stale_sync["freshness"]["status"],
            "freshness_fail_closed": stale_sync["freshness"]["fail_closed"],
            "node_upserts_positive": !stale_sync["node_upserts"].as_array().unwrap().is_empty(),
            "edge_upserts_positive": !stale_sync["edge_upserts"].as_array().unwrap().is_empty(),
            "node_tombstones": stale_sync["node_tombstones"],
            "edge_tombstones": stale_sync["edge_tombstones"],
            "chunk_operations": chunk_operations(&stale_sync),
            "required_index_count": stale_sync["required_indexes"].as_array().unwrap().len(),
            "diagnostics_mention_fail_closed": stale_sync["diagnostics"]
                .as_array()
                .unwrap()
                .iter()
                .any(|diagnostic| diagnostic.as_str().unwrap().contains("fail closed")),
        },
        "conflict_matrix": {
            "contract_version": conflict["contract_version"],
            "targets": conflict["targets"],
            "can_parallel": conflict["can_parallel"],
            "fail_closed": conflict["fail_closed"],
            "cross_target_parallel_safe": conflict["cross_target_parallel_safe"],
            "per_target_fail_closed": conflict["per_target_fail_closed"],
            "candidate_summaries": conflict_candidate_summaries(&conflict),
            "worker_prompt_packets": worker_prompt_packet_summaries(&conflict),
            "evidence_packet_count": conflict["orchestration"]["evidence_packet_ids"].as_array().unwrap().len(),
            "projection_hash_count": conflict["orchestration"]["projection_hashes"].as_array().unwrap().len(),
        },
        "dependency_dag": {
            "contract_version": dependency_dag["contract_version"],
            "targets": dependency_dag["targets"],
            "projection_freshness_status": dependency_dag["projection_freshness"]["status"],
            "projection_hash_count": dependency_dag["projection_hashes"].as_array().unwrap().len(),
            "has_worker_result_follow_up": dependency_dag["edges"]
                .as_array()
                .unwrap()
                .iter()
                .any(|edge| edge["kind"] == "worker_result_follow_up"),
            "has_topo_batches": !dependency_dag["topo_batches"].as_array().unwrap().is_empty(),
            "has_cycles": dependency_dag["cycle_diagnostics"]["has_cycles"],
            "blocked_nodes": dependency_dag["cycle_diagnostics"]["blocked_nodes"],
            "replay_mentions_dependency_dag": commands_contain(&dependency_dag["replay_commands"], "dependency-dag"),
            "repair_mentions_refresh": commands_contain(&dependency_dag["repair_commands"], "refresh"),
            "repair_mentions_doctor": commands_contain(&dependency_dag["repair_commands"], "doctor"),
        },
        "dependency_dag_cycle": {
            "contract_version": dependency_dag_cycle["contract_version"],
            "targets": dependency_dag_cycle["targets"],
            "projection_freshness_status": dependency_dag_cycle["projection_freshness"]["status"],
            "has_cycles": dependency_dag_cycle["cycle_diagnostics"]["has_cycles"],
            "blocked_nodes": dependency_dag_cycle["cycle_diagnostics"]["blocked_nodes"],
            "cycle_edge_kinds": dependency_dag_cycle["cycle_diagnostics"]["cycle_edges"]
                .as_array()
                .unwrap()
                .iter()
                .map(|edge| edge["kind"].clone())
                .collect::<Vec<_>>(),
        },
        "dispatch_trace_json": {
            "contract_version": trace["contract_version"],
            "targets": trace["targets"],
            "projection_hash_count": trace["projection_hashes"].as_array().unwrap().len(),
            "evidence_packet_ids_match_conflict": trace["evidence_packet_ids"] == conflict["orchestration"]["evidence_packet_ids"],
            "worker_prompt_packets": worker_prompt_packet_summaries(&trace),
            "worker_feedback": trace_feedback,
            "node_kind_counts": node_kind_counts(&trace["nodes"]),
            "replay_commands_match_conflict": trace["replay_commands"] == conflict["next_commands"],
            "repair_mentions_refresh": commands_contain(&trace["repair_commands"], "refresh"),
            "repair_mentions_doctor": commands_contain(&trace["repair_commands"], "doctor"),
        },
        "dispatch_trace_html": {
            "contains_graph_canvas": html.contains("id=\"graph-canvas\""),
            "contains_contract_version": html.contains("dispatch-trace-v1"),
            "contains_worker_prompt_packets": html.contains("worker_prompt_packets"),
            "contains_parallel_safe": html.contains("parallel_safe"),
            "contains_follow_up_debt": html.contains("Follow-up debt"),
            "contains_closure": html.contains("closure"),
            "contains_evidence_packet": trace["evidence_packet_ids"]
                .as_array()
                .unwrap()
                .iter()
                .filter_map(|packet| packet.as_str())
                .any(|packet| html.contains(packet)),
        },
        "context_pack": {
            "graph_orchestration_contract_version": context_graph["contract_version"],
            "projection_freshness_status": context_graph["projection_freshness"]["status"],
            "evidence_packet_count": context_graph["evidence_packet_ids"].as_array().unwrap().len(),
            "conflict_matrix_decisions": context_graph["conflict_matrix_decisions"],
            "follow_up_mentions_conflict_matrix": commands_contain(&context_graph["follow_up_commands"], "conflict-matrix"),
            "worker_ownership_block_count": context_graph["worker_ownership_blocks"].as_array().unwrap().len(),
        },
        "session_review": {
            "contract_version": session_review["contract_version"],
            "prompt_target_total": session_review["prompt_target_total"],
            "prompt_targets": session_review["prompt_targets"],
            "next_digest_mentions_conflict_matrix": commands_contain(&session_review["next_digest_commands"], "conflict-matrix"),
            "next_digest_mentions_context_pack": commands_contain(&session_review["next_digest_commands"], "context-pack"),
        },
    })
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

fn assert_graph_db_page_semantics_match(sqlite_page: &Value, convex_page: &Value) {
    for key in [
        "cursor",
        "limit",
        "next_cursor",
        "property_filters",
        "returned_edges",
        "returned_nodes",
        "truncated",
    ] {
        assert_eq!(sqlite_page[key], convex_page[key], "page field {key}");
    }
}

fn assert_sqlite_page_uses_index(page: &Value, index: &str) {
    let diagnostics = page["diagnostics"]
        .as_array()
        .expect("page diagnostics should be an array");
    assert!(
        diagnostics
            .iter()
            .any(|diagnostic| diagnostic.as_str().is_some_and(|raw| raw.contains(index))),
        "expected SQLite page diagnostics to mention {index}, got {diagnostics:?}"
    );
}

fn assert_sqlite_page_diagnostic_contains(page: &Value, expected: &str) {
    let diagnostics = page["diagnostics"]
        .as_array()
        .expect("page diagnostics should be an array");
    assert!(
        diagnostics.iter().any(|diagnostic| diagnostic
            .as_str()
            .is_some_and(|raw| raw.contains(expected))),
        "expected SQLite page diagnostics to mention {expected}, got {diagnostics:?}"
    );
}

fn assert_graph_db_snapshot_query_parity(project: &Path, snapshot: &Path) {
    graph_db_json(project, Backend::Sqlite, vec!["refresh".to_string()]);
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
    assert_graph_db_page_semantics_match(&sqlite_first["page"], &convex_first["page"]);
    assert_sqlite_page_uses_index(&sqlite_first["page"], "idx_graph_nodes_kind");
    assert_sqlite_page_uses_index(
        &sqlite_first["page"],
        "idx_graph_node_properties_key_value_node",
    );
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
    assert_graph_db_page_semantics_match(&sqlite_second["page"], &convex_second["page"]);
    assert_sqlite_page_uses_index(&sqlite_second["page"], "idx_graph_nodes_kind");
    assert_sqlite_page_uses_index(
        &sqlite_second["page"],
        "idx_graph_node_properties_key_value_node",
    );

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

    let neighborhood_center_id = main_id.clone();
    let neighborhood_query = vec![
        "neighborhood".to_string(),
        neighborhood_center_id.clone(),
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
    let ranked_neighbors = sqlite_neighborhood["ranked_neighbors"].as_array().unwrap();
    assert!(!ranked_neighbors.is_empty(), "{sqlite_neighborhood}");
    assert!(
        ranked_neighbors.iter().all(|neighbor| {
            neighbor["node_id"].as_str() != Some(neighborhood_center_id.as_str())
                && neighbor["score"].as_i64().is_some()
                && neighbor["edge_kinds"]
                    .as_array()
                    .is_some_and(|kinds| !kinds.is_empty())
        }),
        "{sqlite_neighborhood}"
    );
    assert!(
        !sqlite_neighborhood["neighborhood_ranking_gate"]["ranked_output_default"]
            .as_bool()
            .unwrap()
    );
    assert_eq!(
        sqlite_neighborhood["neighborhood_ranking_gate"]["default_change_gate"],
        "community_search_quality_metrics"
    );
    assert!(
        sqlite_neighborhood["neighborhood_ranking_gate"]["required_metrics"]
            .as_array()
            .unwrap()
            .iter()
            .any(|metric| metric == "duplicate_name_precision")
    );
    assert_graph_db_page_semantics_match(
        &sqlite_neighborhood["page"],
        &convex_neighborhood["page"],
    );
    assert_sqlite_page_uses_index(&sqlite_neighborhood["page"], "idx_graph_edges_from_kind");
    assert_sqlite_page_diagnostic_contains(
        &sqlite_neighborhood["page"],
        "one recursive reachable-set CTE",
    );

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
            "80".to_string(),
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
        refresh["refresh"]["mode"], "cold_source_graph_rebuild",
        "{refresh}"
    );
    assert_eq!(
        refresh["freshness"]["projection_version"], "tsift-traversal-v1",
        "{refresh}"
    );
    assert!(refresh["freshness"]["content_hash"].as_str().is_some());
    assert!(refresh["freshness"]["source_watermark"].as_str().is_some());
    assert!(refresh["counts"]["nodes"].as_u64().unwrap() > 0);
    assert!(refresh["counts"]["edges"].as_u64().unwrap() > 0);
    assert!(refresh["counts"]["tombstones"]["total"].as_u64().is_some());
    assert!(refresh["compaction"]["live_rows"].as_u64().unwrap() > 0);
    assert!(
        refresh["compaction"]["proof"]
            .as_array()
            .unwrap()
            .iter()
            .any(|proof| proof.as_str().unwrap().contains("retained tombstone")),
        "{refresh}"
    );
    assert!(
        refresh["warnings"]
            .as_array()
            .unwrap()
            .iter()
            .any(|warning| {
                let warning = warning.as_str().unwrap();
                warning.contains("summary cache empty")
                    && warning.contains("tsift summarize --extract")
                    && warning.contains("semantic rows are unavailable")
            }),
        "{refresh}"
    );
    assert_eq!(refresh["readiness"]["status"], "blocked", "{refresh}");
    assert_eq!(refresh["readiness"]["fail_closed"], true, "{refresh}");
    assert_eq!(
        refresh["readiness"]["reason"], "summary_cache_empty",
        "{refresh}"
    );
    assert!(
        refresh["readiness"]["next_commands"]
            .as_array()
            .unwrap()
            .iter()
            .any(|command| command.as_str().unwrap().contains("summarize --extract")),
        "{refresh}"
    );
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

    let cached_refresh =
        graph_db_json(project.path(), Backend::Sqlite, vec!["refresh".to_string()]);
    assert_eq!(cached_refresh["status"], "current", "{cached_refresh}");
    assert_eq!(
        cached_refresh["refresh"]["mode"], "cached_source_watermark_reuse",
        "{cached_refresh}"
    );
    assert_eq!(
        cached_refresh["counts"]["nodes"],
        refresh["counts"]["nodes"]
    );
    assert!(
        cached_refresh["refresh"]["phase_timings"]
            .as_array()
            .unwrap()
            .iter()
            .any(|phase| {
                phase["name"] == "source_graph_build"
                    && phase["detail"]
                        .as_str()
                        .unwrap()
                        .contains("reused current graph.db projection")
            }),
        "{cached_refresh}"
    );

    let status = graph_db_json(project.path(), Backend::Sqlite, vec!["status".to_string()]);
    assert_eq!(status["operation"], "status", "{status}");
    assert_eq!(status["status"], "current", "{status}");
    assert_eq!(status["counts"]["nodes"], refresh["counts"]["nodes"]);
    assert_eq!(
        status["compaction"]["live_rows"],
        refresh["compaction"]["live_rows"]
    );
}

#[test]
fn graph_db_semantic_readiness_clears_after_summarize_extract_and_refresh() {
    let project = graph_db_project();
    init_git_repo(project.path());

    let refresh = graph_db_json(project.path(), Backend::Sqlite, vec!["refresh".to_string()]);
    assert_eq!(refresh["readiness"]["status"], "blocked", "{refresh}");

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
    let empty_neighborhood = graph_db_json(
        project.path(),
        Backend::Sqlite,
        vec![
            "neighborhood".to_string(),
            backlog_id.clone(),
            "--depth".to_string(),
            "4".to_string(),
            "--limit".to_string(),
            "80".to_string(),
        ],
    );
    assert!(
        !empty_neighborhood["nodes"]
            .as_array()
            .unwrap()
            .iter()
            .any(|node| node["kind"]
                .as_str()
                .is_some_and(|kind| kind.starts_with("semantic_"))),
        "{empty_neighborhood}"
    );
    let empty_evidence = graph_db_json(
        project.path(),
        Backend::Sqlite,
        vec![
            "evidence".to_string(),
            "gval".to_string(),
            "--depth".to_string(),
            "4".to_string(),
            "--limit".to_string(),
            "8".to_string(),
        ],
    );
    assert_eq!(
        empty_evidence["semantic_related"].as_array().unwrap().len(),
        0,
        "{empty_evidence}"
    );

    seed_current_semantic_summary(project.path(), "main.rs", "helper");
    let summarize = assert_tsift_json(vec![
        "summarize".to_string(),
        "--path".to_string(),
        project.path().to_string_lossy().to_string(),
        "--extract".to_string(),
        "main.rs".to_string(),
        "--json".to_string(),
    ]);
    assert!(
        summarize["errors"].as_array().unwrap().is_empty(),
        "{summarize}"
    );

    let refreshed = graph_db_json(project.path(), Backend::Sqlite, vec!["refresh".to_string()]);
    assert_eq!(refreshed["readiness"]["status"], "ready", "{refreshed}");
    assert_eq!(
        refreshed["readiness"]["reason"], "semantic_rows_available",
        "{refreshed}"
    );

    let semantic_neighborhood = graph_db_json(
        project.path(),
        Backend::Sqlite,
        vec![
            "neighborhood".to_string(),
            backlog_id,
            "--depth".to_string(),
            "4".to_string(),
            "--limit".to_string(),
            "80".to_string(),
        ],
    );
    assert!(
        semantic_neighborhood["nodes"]
            .as_array()
            .unwrap()
            .iter()
            .any(|node| node["kind"] == "semantic_concept" && node["label"] == "graph navigation"),
        "{semantic_neighborhood}"
    );

    let semantic_evidence = graph_db_json(
        project.path(),
        Backend::Sqlite,
        vec![
            "evidence".to_string(),
            "gval".to_string(),
            "--depth".to_string(),
            "4".to_string(),
            "--limit".to_string(),
            "8".to_string(),
        ],
    );
    assert!(
        semantic_evidence["semantic_related"]
            .as_array()
            .unwrap()
            .iter()
            .any(|node| node["kind"] == "semantic_concept" && node["label"] == "graph navigation"),
        "{semantic_evidence}"
    );

    let session = project.path().join("tasks/software/tsift.md");
    let conflict = assert_tsift_json(vec![
        "conflict-matrix".to_string(),
        "--path".to_string(),
        session.to_string_lossy().to_string(),
        "--json".to_string(),
        "gval".to_string(),
    ]);
    let candidate = &conflict["candidates"].as_array().unwrap()[0];
    assert_eq!(candidate["target"], "gval", "{conflict}");
    assert!(
        candidate["semantic_dispatch_score"].as_u64().unwrap() > 0,
        "{conflict}"
    );
    assert!(
        candidate["semantic_dispatch_reasons"]
            .as_array()
            .unwrap()
            .iter()
            .any(|reason| reason.as_str().unwrap().contains("semantic_concept")),
        "{conflict}"
    );
}

#[test]
fn graph_db_related_json_reports_summary_extract_readiness_gate() {
    let project = graph_db_project();
    let refresh = graph_db_json(project.path(), Backend::Sqlite, vec!["refresh".to_string()]);
    assert_eq!(refresh["readiness"]["reason"], "summary_cache_empty");

    let related = graph_db_json(
        project.path(),
        Backend::Sqlite,
        vec![
            "related".to_string(),
            "graph navigation".to_string(),
            "--limit".to_string(),
            "8".to_string(),
        ],
    );

    assert_eq!(related["readiness"]["status"], "blocked", "{related}");
    assert_eq!(
        related["readiness"]["reason"], "summary_cache_empty",
        "{related}"
    );
    assert_eq!(related["readiness"]["fail_closed"], true, "{related}");
    let next_commands = related["readiness"]["next_commands"].as_array().unwrap();
    assert!(
        next_commands
            .iter()
            .any(|command| command.as_str().unwrap() == "tsift summarize --extract ."),
        "{related}"
    );
    assert!(
        next_commands.iter().any(|command| {
            let command = command.as_str().unwrap();
            command.contains("graph-db --path")
                && command.contains("refresh --json")
                && command.contains(project.path().to_string_lossy().as_ref())
        }),
        "{related}"
    );
    assert!(
        related["knowledge_retrieval"]["diagnostics"]
            .as_array()
            .unwrap()
            .iter()
            .any(|diagnostic| diagnostic.as_str().unwrap().contains("summary cache empty")),
        "{related}"
    );
}

#[test]
fn graph_db_compact_reports_policy_and_guarded_apply() {
    let project = graph_db_project();
    let refresh = graph_db_json(project.path(), Backend::Sqlite, vec!["refresh".to_string()]);

    let dry_run = graph_db_json(project.path(), Backend::Sqlite, vec!["compact".to_string()]);
    assert_eq!(dry_run["applied"], false, "{dry_run}");
    assert_eq!(
        dry_run["counts_before"]["nodes"], refresh["counts"]["nodes"],
        "{dry_run}"
    );
    assert!(
        dry_run["next_commands"]
            .as_array()
            .unwrap()
            .iter()
            .any(|command| command.as_str().unwrap().contains("convex-sync")),
        "{dry_run}"
    );

    let guard = graph_db_failure(
        project.path(),
        Backend::Sqlite,
        vec![
            "compact".to_string(),
            "--apply".to_string(),
            "--prune-tombstones".to_string(),
        ],
    );
    assert!(guard.contains("--confirmed-convex-reconciled"), "{guard}");

    let applied = graph_db_json(
        project.path(),
        Backend::Sqlite,
        vec![
            "compact".to_string(),
            "--apply".to_string(),
            "--prune-tombstones".to_string(),
            "--confirmed-convex-reconciled".to_string(),
        ],
    );
    assert_eq!(applied["applied"], true, "{applied}");
    assert_eq!(applied["counts_after"]["nodes"], refresh["counts"]["nodes"]);
    assert_eq!(
        applied["compaction_after"]["safe_to_prune_tombstones"], true,
        "{applied}"
    );
    assert!(
        applied["next_commands"]
            .as_array()
            .unwrap()
            .iter()
            .any(|command| command.as_str().unwrap().contains("doctor")),
        "{applied}"
    );
}

#[test]
fn graph_db_backend_eval_benchmarks_candidate_stores_against_sqlite() {
    let project = graph_db_project();
    init_git_repo(project.path());
    let session = project.path().join("tasks/software/tsift.md");
    let cache_artifact_path = seed_conflict_matrix_cache_index_artifact(project.path());

    let backend_eval_args = || {
        vec![
            "graph-db".to_string(),
            "--path".to_string(),
            session.to_string_lossy().to_string(),
            "--json".to_string(),
            "backend-eval".to_string(),
            "--candidate".to_string(),
            "duckdb-duckpgq".to_string(),
            "--candidate".to_string(),
            "falkordb".to_string(),
            "--candidate".to_string(),
            "ladybug".to_string(),
            "--candidate".to_string(),
            "kuzu".to_string(),
            "--candidate".to_string(),
            "surrealdb".to_string(),
            "--target".to_string(),
            "gval".to_string(),
        ]
    };

    let report = assert_tsift_json(backend_eval_args());

    assert_eq!(report["baseline_backend"], "sqlite", "{report}");
    assert_eq!(
        report["candidates"],
        json!(["duckdb-duckpgq", "falkordb", "ladybug", "kuzu", "surrealdb"]),
        "{report}"
    );
    let phases = report["phase_timings"].as_array().unwrap();
    for phase in [
        "source_graph_build",
        "projection_rows",
        "sqlite_open",
        "sqlite_property_row_staging",
        "sqlite_edge_property_row_staging",
        "sqlite_delta_write",
        "conflict_matrix_preparation",
        "conflict_matrix_preparation.session_review_compute",
        "conflict_matrix_preparation.session_review_compute.target_context_build",
        "conflict_matrix_preparation.session_review_compute.session_discovery",
        "conflict_matrix_preparation.session_review_compute.session_digest_total",
        "conflict_matrix_preparation.session_review_compute.session_cost_total",
        "conflict_matrix_preparation.session_review_compute.session_aggregation",
        "conflict_matrix_preparation.session_review_compute.report_assembly",
        "conflict_matrix_preparation.status_index_gate.prepare_agent_doc_index_gate",
        "conflict_matrix_preparation.status_index_gate.context_pack_status_reminders",
        "conflict_matrix_preparation.status_index_gate.load_tag_ontology_preview_context",
        "conflict_matrix_preparation.impact.context_resolution",
        "conflict_matrix_preparation.impact.diff_digest",
        "conflict_matrix_preparation.impact.test_path_scan",
        "conflict_matrix_preparation.impact.index_open",
        "conflict_matrix_preparation.impact.call_edge_impacts",
        "conflict_matrix_preparation.impact.route_handler_impacts",
        "conflict_matrix_preparation.impact.import_impacts",
        "conflict_matrix_preparation.impact.report_assembly",
    ] {
        assert!(
            phases.iter().any(|entry| entry["name"] == phase),
            "missing phase {phase}: {report}"
        );
    }
    let session_review_total = phase_duration(
        &report,
        "conflict_matrix_preparation.session_review_compute",
    );
    let session_review_sub_sum: u64 = [
        "conflict_matrix_preparation.session_review_compute.target_context_build",
        "conflict_matrix_preparation.session_review_compute.session_discovery",
        "conflict_matrix_preparation.session_review_compute.session_digest_total",
        "conflict_matrix_preparation.session_review_compute.session_cost_total",
        "conflict_matrix_preparation.session_review_compute.session_aggregation",
        "conflict_matrix_preparation.session_review_compute.report_assembly",
    ]
    .iter()
    .map(|name| phase_duration(&report, name))
    .sum();
    assert!(
        session_review_sub_sum <= session_review_total + 50_000,
        "session_review_compute sub-phase sum {session_review_sub_sum}us should not exceed parent {session_review_total}us by more than 50ms instrumentation slack: {report}"
    );
    assert!(
        session_review_total <= session_review_sub_sum + 50_000,
        "session_review_compute parent {session_review_total}us should not exceed sub-phase sum {session_review_sub_sum}us by more than 50ms instrumentation slack: {report}"
    );
    assert!(
        phase_detail(&report, "source_graph_build").contains("bounded session projection"),
        "session-hinted backend-eval should expose the bounded dirty-scope projection profile: {report}"
    );
    assert_eq!(report["config"]["path_max_hops"], 64, "{report}");
    assert_eq!(report["config"]["path_direct_hop_budget"], 1, "{report}");
    assert_eq!(
        report["config"]["path_deep_chain_hop_budget"], 64,
        "{report}"
    );
    assert_eq!(
        report["config"]["path_extended_hop_budgets"],
        json!([128, 256, 512]),
        "{report}"
    );
    assert!(
        report["config"]["path_hop_policy"]
            .as_str()
            .unwrap()
            .contains("default path reads stay capped at 64 hops"),
        "{report}"
    );
    assert_eq!(
        report["config"]["full_projection_enabled"], false,
        "{report}"
    );
    assert!(
        report["config"]["path_query_plan_checks"]
            .as_array()
            .unwrap()
            .iter()
            .any(|check| check
                .as_str()
                .unwrap()
                .contains("idx_graph_edges_from_kind")),
        "{report}"
    );
    assert!(
        report["config"]["path_probe_strategy"]
            .as_str()
            .unwrap()
            .contains("128/256/512-hop"),
        "{report}"
    );
    assert_eq!(report["config"]["normalization_row_unit"], 1000, "{report}");
    let datasets = report["datasets"].as_array().unwrap();
    assert_eq!(datasets.len(), 3, "{report}");
    assert!(
        datasets.iter().any(|dataset| dataset["name"] == "real"),
        "{report}"
    );
    assert!(
        datasets
            .iter()
            .any(|dataset| dataset["name"] == "synthetic_high_degree"),
        "{report}"
    );
    assert!(
        datasets
            .iter()
            .any(|dataset| dataset["name"] == "synthetic_deep_chain"),
        "{report}"
    );
    let surrealdb_parity_operations = [
        "status",
        "edge_lookup",
        "edge_property_scan",
        "incident_edges",
        "neighborhood",
        "related",
        "path_max_hops",
        "path_max_hops_128",
        "path_max_hops_256",
        "path_max_hops_512",
        "evidence_target_resolution",
        "evidence",
        "conflict_matrix",
        "dispatch_trace",
    ];
    for dataset in datasets {
        assert!(dataset["nodes"].as_u64().unwrap() > 0, "{report}");
        assert!(dataset["edges"].as_u64().unwrap() > 0, "{report}");
        let backends = dataset["backends"].as_array().unwrap();
        assert_eq!(backends.len(), 6, "{report}");
        for backend in backends {
            let operations = backend["operations"]
                .as_array()
                .unwrap()
                .iter()
                .map(|operation| operation["name"].as_str().unwrap().to_string())
                .collect::<Vec<_>>();
            assert_eq!(
                operations,
                vec![
                    "refresh".to_string(),
                    "status".to_string(),
                    "edge_lookup".to_string(),
                    "edge_property_scan".to_string(),
                    "incident_edges".to_string(),
                    "neighborhood".to_string(),
                    "related".to_string(),
                    "path_max_hops".to_string(),
                    "path_max_hops_128".to_string(),
                    "path_max_hops_256".to_string(),
                    "path_max_hops_512".to_string(),
                    "evidence_target_resolution".to_string(),
                    "evidence".to_string(),
                    "conflict_matrix".to_string(),
                    "dispatch_trace".to_string(),
                ],
                "{report}"
            );
            assert!(
                backend["operations"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .all(|operation| operation["status"] == "ok"),
                "{report}"
            );
            if backend["backend"] != "sqlite" {
                let expected_read_only =
                    !(backend["backend"] == "surrealdb" && cfg!(feature = "backend-surrealdb"));
                assert_eq!(backend["read_only"], expected_read_only, "{report}");
                assert_eq!(backend["parity"]["matches_sqlite"], true, "{report}");
            }
            if backend["backend"] == "kuzu" {
                assert!(
                    backend["adapter"]
                        .as_str()
                        .unwrap()
                        .contains("Vela-Engineering/kuzu"),
                    "{report}"
                );
                assert!(
                    backend["projection_load"]
                        .as_str()
                        .unwrap()
                        .contains("Kuzu-compatible"),
                    "{report}"
                );
                assert!(
                    backend["lock_behavior"]
                        .as_str()
                        .unwrap()
                        .contains("concurrent writer"),
                    "{report}"
                );
                assert!(
                    backend["install_portability"]
                        .as_str()
                        .unwrap()
                        .contains("cargo build/install"),
                    "{report}"
                );
            }
            if backend["backend"] == "falkordb" {
                assert!(
                    backend["projection_load"]
                        .as_str()
                        .unwrap()
                        .contains("production FalkorDB"),
                    "{report}"
                );
                assert!(
                    backend["lock_behavior"]
                        .as_str()
                        .unwrap()
                        .contains("multi-process writer"),
                    "{report}"
                );
            }
            if backend["backend"] == "surrealdb" {
                assert!(
                    backend["adapter"].as_str().unwrap().contains("SurrealDB"),
                    "{report}"
                );
                assert!(
                    backend["projection_load"].as_str().unwrap().contains(
                        if cfg!(feature = "backend-surrealdb") {
                            "file-backed SurrealDB"
                        } else {
                            "SurrealDB-compatible"
                        }
                    ),
                    "{report}"
                );
                if cfg!(feature = "backend-surrealdb") {
                    assert!(
                        backend["adapter"].as_str().unwrap().contains("SurrealKV"),
                        "{report}"
                    );
                }
                assert!(
                    backend["lock_behavior"]
                        .as_str()
                        .unwrap()
                        .contains("file-backed writer"),
                    "{report}"
                );
                assert!(
                    backend["install_portability"]
                        .as_str()
                        .unwrap()
                        .contains("cargo build/install"),
                    "{report}"
                );
            }
        }
        let dataset_name = dataset["name"].as_str().unwrap();
        let sqlite_backend = backend_eval_backend(dataset, "sqlite", &report);
        let surrealdb_backend = backend_eval_backend(dataset, "surrealdb", &report);
        assert_eq!(
            surrealdb_backend["read_only"],
            !cfg!(feature = "backend-surrealdb"),
            "{report}"
        );
        assert_eq!(
            surrealdb_backend["parity"]["matches_sqlite"], true,
            "{report}"
        );
        assert!(
            surrealdb_backend["parity"]["diagnostics"]
                .as_array()
                .unwrap()
                .is_empty(),
            "{report}"
        );
        assert_backend_eval_metric(
            &report,
            &format!("{dataset_name}.surrealdb.total_duration_micros"),
        );
        assert_backend_eval_metric(
            &report,
            &format!("{dataset_name}.surrealdb.total_duration_micros_per_1k_graph_rows"),
        );
        let status = backend_eval_operation(surrealdb_backend, "status", &report);
        assert_eq!(
            status["rows"].as_u64().unwrap(),
            dataset["nodes"].as_u64().unwrap() + dataset["edges"].as_u64().unwrap(),
            "SurrealDB status/count rows should match dataset graph rows: {report}"
        );
        for operation in surrealdb_parity_operations {
            let sqlite_operation = backend_eval_operation(sqlite_backend, operation, &report);
            let surrealdb_operation = backend_eval_operation(surrealdb_backend, operation, &report);
            assert_eq!(
                sqlite_operation["status"], "ok",
                "SQLite baseline operation {operation} must pass before SurrealDB parity is meaningful: {report}"
            );
            assert_eq!(
                surrealdb_operation["status"], "ok",
                "SurrealDB operation {operation} should pass on dataset {dataset_name}: {report}"
            );
            assert_eq!(
                surrealdb_operation["rows"], sqlite_operation["rows"],
                "SurrealDB operation {operation} should return SQLite-equivalent row counts on dataset {dataset_name}: {report}"
            );
            assert_backend_eval_metric(
                &report,
                &format!("{dataset_name}.surrealdb.{operation}.duration_micros"),
            );
            assert_backend_eval_metric(
                &report,
                &format!("{dataset_name}.surrealdb.{operation}.duration_micros_per_1k_graph_rows"),
            );
            if !sqlite_operation["rows"].is_null() {
                assert_backend_eval_metric(
                    &report,
                    &format!("{dataset_name}.surrealdb.{operation}.rows"),
                );
            }
        }
    }
    assert_eq!(report["promotion"].as_array().unwrap().len(), 5, "{report}");
    assert!(
        report["promotion"]
            .as_array()
            .unwrap()
            .iter()
            .all(|decision| {
                decision["decision"] == "eligible" || decision["decision"] == "hold"
            }),
        "{report}"
    );
    assert_eq!(
        report["performance_gate"]["baseline_fixture"],
        "fixtures/graph-db-performance-history.json",
        "{report}"
    );
    assert_eq!(
        report["performance_gate"]["allowed_regression_percent"],
        json!(10.0),
        "{report}"
    );
    assert_eq!(
        report["performance_gate"]["minimum_sample_runs"], 3,
        "{report}"
    );
    assert_eq!(
        report["performance_gate"]["normalized_metric_unit"], "duration_micros_per_1k_graph_rows",
        "{report}"
    );
    assert!(
        report["performance_gate"]["required_metrics"]
            .as_array()
            .unwrap()
            .iter()
            .any(|metric| metric == "real.refresh_phase.sqlite_delta_write.duration_micros"),
        "{report}"
    );
    assert!(
        report["performance_gate"]["required_metrics"]
            .as_array()
            .unwrap()
            .iter()
            .any(
                |metric| metric == "real.refresh_phase.sqlite_property_row_staging.duration_micros"
            ),
        "{report}"
    );
    assert!(
        report["performance_gate"]["required_metrics"]
            .as_array()
            .unwrap()
            .iter()
            .any(|metric| metric
                == "real.refresh_phase.sqlite_edge_property_row_staging.duration_micros"),
        "{report}"
    );
    assert!(
        report["performance_gate"]["required_metrics"]
            .as_array()
            .unwrap()
            .iter()
            .any(|metric| metric == "real.sqlite.edge_lookup.duration_micros_per_1k_graph_rows"),
        "{report}"
    );
    assert!(
        report["performance_gate"]["required_metrics"]
            .as_array()
            .unwrap()
            .iter()
            .any(|metric| metric
                == "real.sqlite.evidence_target_resolution.duration_micros_per_1k_graph_rows"),
        "{report}"
    );
    assert!(
        report["performance_gate"]["required_metrics"]
            .as_array()
            .unwrap()
            .iter()
            .any(|metric| metric
                == "real.refresh_phase.source_graph_build.duration_micros_per_1k_graph_rows"),
        "{report}"
    );
    assert!(
        report["metrics"]
            .as_object()
            .unwrap()
            .contains_key("real.sqlite.total_duration_micros"),
        "{report}"
    );
    assert!(
        report["metrics"]
            .as_object()
            .unwrap()
            .contains_key("real.graph_rows"),
        "{report}"
    );
    assert!(
        report["metrics"]
            .as_object()
            .unwrap()
            .contains_key("real.sqlite.total_duration_micros_per_1k_graph_rows"),
        "{report}"
    );
    assert!(
        report["metrics"]
            .as_object()
            .unwrap()
            .contains_key("real.sqlite.evidence.duration_micros_per_1k_graph_rows"),
        "{report}"
    );
    assert!(
        report["metrics"]
            .as_object()
            .unwrap()
            .contains_key("real.sqlite.edge_lookup.duration_micros_per_1k_graph_rows"),
        "{report}"
    );
    assert!(
        report["metrics"]
            .as_object()
            .unwrap()
            .contains_key("real.sqlite.edge_property_scan.duration_micros_per_1k_graph_rows"),
        "{report}"
    );
    assert!(
        report["metrics"]
            .as_object()
            .unwrap()
            .contains_key("real.sqlite.incident_edges.duration_micros_per_1k_graph_rows"),
        "{report}"
    );
    assert!(
        report["metrics"]
            .as_object()
            .unwrap()
            .contains_key("real.sqlite.neighborhood.duration_micros_per_1k_graph_rows"),
        "{report}"
    );
    assert!(
        report["metrics"]
            .as_object()
            .unwrap()
            .contains_key("real.sqlite.evidence_target_resolution.duration_micros"),
        "{report}"
    );
    assert!(
        report["metrics"].as_object().unwrap().contains_key(
            "real.sqlite.evidence_target_resolution.duration_micros_per_1k_graph_rows"
        ),
        "{report}"
    );
    assert!(
        report["metrics"]
            .as_object()
            .unwrap()
            .contains_key("synthetic_deep_chain.sqlite.path_max_hops.duration_micros"),
        "{report}"
    );
    assert!(
        report["metrics"]
            .as_object()
            .unwrap()
            .contains_key("synthetic_deep_chain.sqlite.path_max_hops_128.duration_micros"),
        "{report}"
    );
    assert!(
        report["metrics"]
            .as_object()
            .unwrap()
            .contains_key("synthetic_deep_chain.sqlite.path_max_hops_256.duration_micros"),
        "{report}"
    );
    assert!(
        report["metrics"]
            .as_object()
            .unwrap()
            .contains_key("synthetic_deep_chain.sqlite.path_max_hops_512.duration_micros"),
        "{report}"
    );
    assert!(
        report["metrics"].as_object().unwrap().contains_key(
            "synthetic_high_degree.sqlite.evidence_target_resolution.duration_micros_per_1k_graph_rows"
        ),
        "{report}"
    );
    assert!(
        report["metrics"].as_object().unwrap().contains_key(
            "synthetic_high_degree.sqlite.neighborhood.duration_micros_per_1k_graph_rows"
        ),
        "{report}"
    );
    assert!(
        report["metrics"].as_object().unwrap().contains_key(
            "synthetic_deep_chain.sqlite.evidence_target_resolution.duration_micros_per_1k_graph_rows"
        ),
        "{report}"
    );
    assert!(
        report["metrics"].as_object().unwrap().contains_key(
            "synthetic_deep_chain.sqlite.neighborhood.duration_micros_per_1k_graph_rows"
        ),
        "{report}"
    );
    assert!(
        report["metrics"].as_object().unwrap().contains_key(
            "synthetic_deep_chain.sqlite.path_max_hops.duration_micros_per_1k_graph_rows"
        ),
        "{report}"
    );
    assert!(
        report["metrics"].as_object().unwrap().contains_key(
            "synthetic_deep_chain.sqlite.path_max_hops_512.duration_micros_per_1k_graph_rows"
        ),
        "{report}"
    );
    assert!(
        report["metrics"]
            .as_object()
            .unwrap()
            .contains_key("real.refresh_phase.sqlite_delta_write.duration_micros"),
        "{report}"
    );
    assert!(
        report["metrics"]
            .as_object()
            .unwrap()
            .contains_key("real.refresh_phase.sqlite_property_row_staging.duration_micros"),
        "{report}"
    );
    assert!(
        report["metrics"]
            .as_object()
            .unwrap()
            .contains_key("real.refresh_phase.sqlite_edge_property_row_staging.duration_micros"),
        "{report}"
    );
    assert!(
        report["metrics"].as_object().unwrap().contains_key(
            "real.refresh_phase.source_graph_build.duration_micros_per_1k_graph_rows"
        ),
        "{report}"
    );
    assert!(
        report["metrics"]
            .as_object()
            .unwrap()
            .contains_key("synthetic_high_degree.kuzu.total_duration_micros"),
        "{report}"
    );
    assert!(
        report["metrics"]
            .as_object()
            .unwrap()
            .contains_key("synthetic_high_degree.surrealdb.total_duration_micros"),
        "{report}"
    );
    assert!(
        report["metric_digest_command"]
            .as_str()
            .unwrap()
            .contains("metric-digest --baseline fixtures/graph-db-performance-history.json"),
        "{report}"
    );
    assert!(
        report["performance_gate"]["repeated_sample_command"]
            .as_str()
            .unwrap()
            .contains("for sample in 1 2 3"),
        "{report}"
    );
    assert_eq!(
        report["performance_gate"]["hop_cap_promotion"]["status"],
        "hold_64_default_until_gate_passes",
        "{report}"
    );
    assert_eq!(
        report["performance_gate"]["hop_cap_promotion"]["current_default_hops"], 64,
        "{report}"
    );
    assert_eq!(
        report["performance_gate"]["hop_cap_promotion"]["candidate_hop_tiers"],
        json!([128, 256, 512]),
        "{report}"
    );
    assert_eq!(
        report["performance_gate"]["hop_cap_promotion"]["required_workloads"],
        json!(["real", "full_projection", "synthetic_deep_chain"]),
        "{report}"
    );
    for workload in ["real", "full_projection", "synthetic_deep_chain"] {
        for hops in ["128", "256", "512"] {
            for leaf in ["duration_micros", "rows"] {
                let metric = format!("{workload}.sqlite.path_max_hops_{hops}.{leaf}");
                assert!(
                    report["performance_gate"]["hop_cap_promotion"]["required_metrics"]
                        .as_array()
                        .unwrap()
                        .iter()
                        .any(|candidate| candidate == &metric),
                    "missing hop-cap promotion metric {metric}: {report}"
                );
            }
        }
    }
    assert!(
        report["performance_gate"]["hop_cap_promotion"]["required_metrics"]
            .as_array()
            .unwrap()
            .iter()
            .any(|metric| metric == "full_projection.sqlite.path_max_hops_512.rows"),
        "{report}"
    );
    assert!(
        report["performance_gate"]["hop_cap_promotion"]["decision_rule"]
            .as_str()
            .unwrap()
            .contains("returning useful path rows"),
        "{report}"
    );
    let adapter_spike = &report["performance_gate"]["backend_adapter_spike"];
    assert_eq!(
        adapter_spike["status"], "hold_real_optional_adapter_required",
        "{report}"
    );
    assert_eq!(
        adapter_spike["evidence_plan"], "plans/gback-evidence.md",
        "{report}"
    );
    assert_eq!(
        adapter_spike["required_workloads"],
        json!([
            "real",
            "full_projection",
            "synthetic_high_degree",
            "synthetic_deep_chain"
        ]),
        "{report}"
    );
    for required_check in [
        "projection_load_writes_provider_neutral_rows_without_sqlite_row_replay",
        "freshness_and_full_parity_match_sqlite_on_every_graphstore_operation",
        "lock_semantics_match_or_beat_sqlite_for_writer_and_read_only_workflows",
        "install_portability_preserves_cargo_build_install_without_external_service_or_native_toolchain",
        "full_projection_cache_hit_sample_before_backend_or_hop_cap_changes",
        "beats_sqlite_on_every_required_workload_and_metric_in_backend_eval",
    ] {
        assert!(
            adapter_spike["required_checks"]
                .as_array()
                .unwrap()
                .iter()
                .any(|check| check == required_check),
            "adapter spike gate should require {required_check}: {report}"
        );
    }
    assert!(
        adapter_spike["decision_rule"]
            .as_str()
            .unwrap()
            .contains("do not promote a read-only prototype"),
        "{report}"
    );
    for backend in ["falkordb", "kuzu", "surrealdb"] {
        let candidate = adapter_spike["candidate_backends"]
            .as_array()
            .unwrap()
            .iter()
            .find(|candidate| candidate["backend"] == backend)
            .unwrap_or_else(|| {
                panic!("missing backend adapter spike candidate for {backend}: {report}")
            });
        let projection_load = candidate["projection_load"]
            .as_str()
            .unwrap()
            .to_lowercase();
        assert!(
            projection_load.contains(backend) || projection_load.contains("kuzu-compatible"),
            "{report}"
        );
        assert!(
            candidate["lock_behavior"]
                .as_str()
                .unwrap()
                .contains("writer"),
            "{report}"
        );
        assert!(
            candidate["install_portability"]
                .as_str()
                .unwrap()
                .contains("cargo build/install"),
            "{report}"
        );
    }
    assert!(
        report["performance_gate"]["required_metrics"]
            .as_array()
            .unwrap()
            .iter()
            .any(|metric| metric == "real.sqlite.neighborhood.duration_micros_per_1k_graph_rows"),
        "{report}"
    );
    assert!(
        report["performance_gate"]["required_metrics"]
            .as_array()
            .unwrap()
            .iter()
            .any(|metric| metric
                == "real.sqlite.path_max_hops_512.duration_micros_per_1k_graph_rows"),
        "{report}"
    );
    assert!(
        report["performance_gate"]["required_metrics"]
            .as_array()
            .unwrap()
            .iter()
            .any(|metric| metric
                == "synthetic_high_degree.sqlite.neighborhood.duration_micros_per_1k_graph_rows"),
        "{report}"
    );
    assert!(
        report["performance_gate"]["required_metrics"]
            .as_array()
            .unwrap()
            .iter()
            .any(|metric| metric
                == "synthetic_deep_chain.sqlite.neighborhood.duration_micros_per_1k_graph_rows"),
        "{report}"
    );
    assert!(
        report["performance_gate"]["required_metrics"]
            .as_array()
            .unwrap()
            .iter()
            .any(|metric| metric
                == "synthetic_deep_chain.sqlite.path_max_hops_256.duration_micros"),
        "{report}"
    );
    assert!(
        report["performance_gate"]["required_metrics"]
            .as_array()
            .unwrap()
            .iter()
            .any(|metric| metric
                == "synthetic_deep_chain.sqlite.path_max_hops_512.duration_micros"),
        "{report}"
    );
    assert!(
        report["performance_gate"]["full_projection_cache_hit_gate"]
            .as_str()
            .unwrap()
            .contains("not evaluated"),
        "{report}"
    );
    assert!(
        report["promotion"]
            .as_array()
            .unwrap()
            .iter()
            .find(|decision| decision["backend"] == "falkordb")
            .unwrap()["reasons"]
            .as_array()
            .unwrap()
            .iter()
            .any(|reason| reason
                .as_str()
                .unwrap()
                .contains("full_projection conflict-matrix")),
        "{report}"
    );
    for backend in ["duckdb-duckpgq", "falkordb", "ladybug", "kuzu", "surrealdb"] {
        let decision = report["promotion"]
            .as_array()
            .unwrap()
            .iter()
            .find(|decision| decision["backend"] == backend)
            .unwrap_or_else(|| panic!("missing promotion decision for {backend}: {report}"));
        assert_eq!(decision["decision"], "hold", "{report}");
        assert_eq!(
            decision["gate"]["status"], "hold_native_adapter_required",
            "{report}"
        );
        assert_eq!(
            decision["gate"]["native_adapter_required"], true,
            "{report}"
        );
    }
    let falkor_gate = &report["promotion"]
        .as_array()
        .unwrap()
        .iter()
        .find(|decision| decision["backend"] == "falkordb")
        .unwrap()["gate"];
    assert!(
        falkor_gate["required_checks"]
            .as_array()
            .unwrap()
            .iter()
            .any(|check| check
                .as_str()
                .unwrap()
                .contains("native_falkordb_projection_load")),
        "{report}"
    );
    let kuzu_gate = &report["promotion"]
        .as_array()
        .unwrap()
        .iter()
        .find(|decision| decision["backend"] == "kuzu")
        .unwrap()["gate"];
    assert_eq!(
        kuzu_gate["status"], "hold_native_adapter_required",
        "{report}"
    );
    assert_eq!(kuzu_gate["native_adapter_required"], true, "{report}");
    assert!(
        kuzu_gate["required_checks"]
            .as_array()
            .unwrap()
            .iter()
            .any(|check| check
                .as_str()
                .unwrap()
                .contains("native_kuzu_projection_load")),
        "{report}"
    );
    let surreal_gate = &report["promotion"]
        .as_array()
        .unwrap()
        .iter()
        .find(|decision| decision["backend"] == "surrealdb")
        .unwrap()["gate"];
    assert_eq!(
        surreal_gate["status"], "hold_native_adapter_required",
        "{report}"
    );
    assert_eq!(surreal_gate["native_adapter_required"], true, "{report}");
    assert!(
        surreal_gate["required_checks"]
            .as_array()
            .unwrap()
            .iter()
            .any(|check| check
                .as_str()
                .unwrap()
                .contains("native_surrealdb_projection_load")),
        "{report}"
    );
    let artifact_file_query = graph_db_json(
        &session,
        Backend::Sqlite,
        vec![
            "kind".to_string(),
            "file".to_string(),
            "--property".to_string(),
            format!("path={cache_artifact_path}"),
            "--limit".to_string(),
            "1".to_string(),
        ],
    );
    assert!(
        graph_db_nodes_empty(&artifact_file_query),
        "conflict-matrix cache artifacts must not become indexed graph file inputs: {artifact_file_query}"
    );
    let artifact_symbol_query = graph_db_json(
        &session,
        Backend::Sqlite,
        vec![
            "kind".to_string(),
            "symbol".to_string(),
            "--property".to_string(),
            format!("path={cache_artifact_path}"),
            "--limit".to_string(),
            "1".to_string(),
        ],
    );
    assert!(
        graph_db_nodes_empty(&artifact_symbol_query),
        "conflict-matrix cache artifacts must not become indexed graph symbol inputs: {artifact_symbol_query}"
    );
    let cached_report = assert_tsift_json(backend_eval_args());
    let cached_source_phase = cached_report["phase_timings"]
        .as_array()
        .unwrap()
        .iter()
        .find(|entry| entry["name"] == "source_graph_build")
        .unwrap();
    assert!(
        cached_source_phase["detail"]
            .as_str()
            .unwrap()
            .contains("reused current graph.db projection"),
        "{cached_report}"
    );
    assert!(
        !cached_report["phase_timings"]
            .as_array()
            .unwrap()
            .iter()
            .any(|entry| entry["name"] == "sqlite_property_row_staging"),
        "cached backend-eval should skip SQLite row staging: {cached_report}"
    );
    assert!(
        cached_report["phase_timings"]
            .as_array()
            .unwrap()
            .iter()
            .any(|entry| entry["name"] == "conflict_matrix_preparation.preparation_cache_lookup"),
        "cached backend-eval should expose preparation cache lookup timing: {cached_report}"
    );
    for phase in [
        "conflict_matrix_preparation.session_review_compute",
        "conflict_matrix_preparation.session_review_compute.target_context_build",
        "conflict_matrix_preparation.session_review_compute.session_discovery",
        "conflict_matrix_preparation.session_review_compute.session_digest_total",
        "conflict_matrix_preparation.session_review_compute.session_cost_total",
        "conflict_matrix_preparation.session_review_compute.session_aggregation",
        "conflict_matrix_preparation.session_review_compute.report_assembly",
        "conflict_matrix_preparation.status_index_gate",
        "conflict_matrix_preparation.status_index_gate.prepare_agent_doc_index_gate",
        "conflict_matrix_preparation.status_index_gate.context_pack_status_reminders",
        "conflict_matrix_preparation.status_index_gate.load_tag_ontology_preview_context",
        "conflict_matrix_preparation.context_pack_diff",
        "conflict_matrix_preparation.impact",
        "conflict_matrix_preparation.impact.context_resolution",
        "conflict_matrix_preparation.impact.diff_digest",
        "conflict_matrix_preparation.impact.test_path_scan",
        "conflict_matrix_preparation.impact.index_open",
        "conflict_matrix_preparation.impact.call_edge_impacts",
        "conflict_matrix_preparation.impact.route_handler_impacts",
        "conflict_matrix_preparation.impact.import_impacts",
        "conflict_matrix_preparation.impact.report_assembly",
    ] {
        assert_eq!(
            phase_duration(&cached_report, phase),
            0,
            "cached backend-eval should report {phase} as skipped on cache hit: {cached_report}"
        );
        assert!(
            phase_detail(&cached_report, phase).contains("source/document/staged-diff watermark"),
            "cached backend-eval should retain freshness guard detail for {phase}: {cached_report}"
        );
    }
    assert!(
        phase_detail(
            &cached_report,
            "conflict_matrix_preparation.preparation_cache_lookup"
        )
        .contains(".tsift/conflict-matrix-cache"),
        "second backend-eval run should hit the conflict-matrix disk cache: {cached_report}"
    );
    let third_cached_report = assert_tsift_json(backend_eval_args());
    assert!(
        phase_detail(&third_cached_report, "source_graph_build")
            .contains("reused current graph.db projection"),
        "{third_cached_report}"
    );
    assert!(
        phase_detail(
            &third_cached_report,
            "conflict_matrix_preparation.preparation_cache_lookup"
        )
        .contains(".tsift/conflict-matrix-cache"),
        "third backend-eval run should keep hitting the same conflict-matrix disk cache instead of self-invalidating: {third_cached_report}"
    );

    let mut full_projection_args = backend_eval_args();
    full_projection_args.push("--full-projection".to_string());
    let full_projection_report = assert_tsift_json(full_projection_args.clone());
    assert_eq!(
        full_projection_report["config"]["full_projection_enabled"], true,
        "{full_projection_report}"
    );
    assert!(
        full_projection_report["datasets"]
            .as_array()
            .unwrap()
            .iter()
            .any(|dataset| dataset["name"] == "full_projection"),
        "{full_projection_report}"
    );
    for phase in [
        "full_projection.cache_lookup",
        "full_projection.source_graph_build",
        "full_projection.projection_rows",
    ] {
        assert!(
            full_projection_report["phase_timings"]
                .as_array()
                .unwrap()
                .iter()
                .any(|entry| entry["name"] == phase),
            "missing phase {phase}: {full_projection_report}"
        );
    }
    for metric in [
        "full_projection.cache.hit",
        "full_projection.cache.disk_bytes",
        "full_projection.cache.compression_ratio",
        "full_projection.refresh_phase.cache_lookup.duration_micros",
        "full_projection.refresh_phase.source_graph_build.duration_micros_per_1k_graph_rows",
        "full_projection.refresh_phase.projection_rows.duration_micros_per_1k_graph_rows",
        "full_projection.sqlite.sqlite_delta_write.duration_micros",
        "full_projection.sqlite.sqlite_node_staging.duration_micros",
        "full_projection.sqlite.post_write_reads.duration_micros",
        "full_projection.sqlite.neighborhood.duration_micros",
        "full_projection.sqlite.evidence_target_resolution.duration_micros",
        "full_projection.sqlite.evidence.duration_micros",
        "full_projection.sqlite.path_max_hops.duration_micros",
        "full_projection.sqlite.path_max_hops_128.duration_micros",
        "full_projection.sqlite.path_max_hops_256.duration_micros",
        "full_projection.sqlite.path_max_hops_512.duration_micros",
        "full_projection.sqlite.total_duration_micros_per_1k_graph_rows",
        "full_projection.sqlite.conflict_matrix.duration_micros",
        "full_projection.sqlite.dispatch_trace.duration_micros",
    ] {
        assert!(
            full_projection_report["metrics"]
                .as_object()
                .unwrap()
                .contains_key(metric),
            "full-projection report should include {metric}: {full_projection_report}"
        );
        assert!(
            full_projection_report["performance_gate"]["required_metrics"]
                .as_array()
                .unwrap()
                .iter()
                .any(|required| required == metric),
            "full-projection performance gate should require {metric}: {full_projection_report}"
        );
    }
    assert!(
        full_projection_report["metric_digest_command"]
            .as_str()
            .unwrap()
            .contains("--full-projection"),
        "{full_projection_report}"
    );
    assert!(
        full_projection_report["performance_gate"]["repeated_sample_command"]
            .as_str()
            .unwrap()
            .contains("--full-projection"),
        "{full_projection_report}"
    );
    assert!(
        full_projection_report["performance_gate"]["full_projection_cache_hit_gate"]
            .as_str()
            .unwrap()
            .contains("full_projection.cache.hit=1"),
        "{full_projection_report}"
    );
    assert!(
        full_projection_report["metrics"]["full_projection.cache.disk_bytes"]
            .as_f64()
            .unwrap()
            > 0.0,
        "{full_projection_report}"
    );
    assert!(
        full_projection_report["metrics"]["full_projection.cache.compression_ratio"]
            .as_f64()
            .unwrap()
            < 1.0,
        "{full_projection_report}"
    );
    let stale_cache_dir = project
        .path()
        .join(".tsift/backend-eval-cache/full_projection");
    fs::create_dir_all(&stale_cache_dir).unwrap();
    let stale_legacy_cache = stale_cache_dir.join("pre-fix-stale.json");
    let stale_compressed_cache = stale_cache_dir.join("pre-fix-stale.json.gz");
    fs::write(&stale_legacy_cache, "{}").unwrap();
    fs::write(&stale_compressed_cache, b"not a valid current cache").unwrap();
    let agent_doc_artifact = project
        .path()
        .join(".agent-doc/baselines/generated-session.md");
    fs::create_dir_all(agent_doc_artifact.parent().unwrap()).unwrap();
    fs::write(
        &agent_doc_artifact,
        "# Generated agent-doc runtime snapshot\n\n<!-- agent:backlog -->\n- [ ] [#noise] Runtime artifact that must not invalidate backend-eval cache.\n<!-- /agent:backlog -->\n",
    )
    .unwrap();
    let cached_full_projection_report = assert_tsift_json(full_projection_args);
    assert!(
        phase_detail(
            &cached_full_projection_report,
            "full_projection.cache.file_read"
        )
        .contains(".tsift/backend-eval-cache"),
        "second full-projection run should hit the source-watermark cache and emit a cache.file_read phase: {cached_full_projection_report}"
    );
    assert!(
        phase_detail(
            &cached_full_projection_report,
            "full_projection.source_graph_build"
        )
        .contains("reused cached full-project source graph"),
        "{cached_full_projection_report}"
    );
    for phase in [
        "full_projection.source_graph_build",
        "full_projection.projection_rows",
    ] {
        assert_eq!(
            phase_duration(&cached_full_projection_report, phase),
            0,
            "full-projection cache hits must skip {phase}: {cached_full_projection_report}"
        );
    }
    assert!(
        !stale_legacy_cache.exists() && !stale_compressed_cache.exists(),
        "full-projection cache hit should prune stale pre-fix artifacts"
    );
    let cache_entries = fs::read_dir(&stale_cache_dir)
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .collect::<Vec<_>>();
    assert_eq!(
        cache_entries.len(),
        1,
        "full-projection cache should keep one source-watermark entry: {cache_entries:?}"
    );
    assert!(
        cache_entries[0]
            .file_name()
            .unwrap()
            .to_string_lossy()
            .ends_with(".json.gz"),
        "full-projection cache should store compressed JSON: {cache_entries:?}"
    );
    assert_eq!(
        cached_full_projection_report["metrics"]["full_projection.cache.hit"], 1.0,
        "{cached_full_projection_report}"
    );
    assert!(
        cached_full_projection_report["metrics"]["full_projection.cache.pruned_files"]
            .as_f64()
            .unwrap()
            >= 2.0,
        "{cached_full_projection_report}"
    );
    for cache_hit_phase in [
        "full_projection.cache.file_read",
        "full_projection.cache.gzip_decode",
        "full_projection.cache.serde_decode",
        "full_projection.cache.prune",
        "full_projection.sqlite.in_memory_open",
        "full_projection.sqlite.replace_projection_total",
        "full_projection.sqlite.post_write_reads",
    ] {
        assert!(
            cached_full_projection_report["phase_timings"]
                .as_array()
                .unwrap()
                .iter()
                .any(|entry| entry["name"] == cache_hit_phase),
            "cache-hit run should emit {cache_hit_phase}: {cached_full_projection_report}"
        );
    }

    let digest_dir = tempfile::tempdir().unwrap();
    let current_report = digest_dir.path().join("backend-eval.json");
    fs::write(&current_report, serde_json::to_vec(&report).unwrap()).unwrap();
    let baseline = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("fixtures/graph-db-performance-history.json");
    let digest = assert_tsift_json(vec![
        "metric-digest".to_string(),
        "--input".to_string(),
        current_report.to_string_lossy().to_string(),
        "--baseline".to_string(),
        baseline.to_string_lossy().to_string(),
        "--json".to_string(),
    ]);
    let delta_metrics = digest["metric_deltas"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|delta| delta["metric"].as_str())
        .collect::<BTreeSet<_>>();
    for metric in [
        "real.sqlite.total_duration_micros",
        "real.sqlite.total_duration_micros_per_1k_graph_rows",
        "real.sqlite.evidence.duration_micros_per_1k_graph_rows",
        "real.sqlite.edge_lookup.duration_micros_per_1k_graph_rows",
        "real.sqlite.edge_property_scan.duration_micros_per_1k_graph_rows",
        "real.sqlite.incident_edges.duration_micros_per_1k_graph_rows",
        "real.refresh_phase.source_graph_build.duration_micros_per_1k_graph_rows",
        "synthetic_high_degree.sqlite.total_duration_micros",
        "synthetic_high_degree.sqlite.total_duration_micros_per_1k_graph_rows",
        "synthetic_deep_chain.sqlite.path_max_hops.duration_micros",
        "synthetic_deep_chain.sqlite.path_max_hops_128.duration_micros",
        "synthetic_deep_chain.sqlite.path_max_hops_256.duration_micros",
        "synthetic_deep_chain.sqlite.path_max_hops_512.duration_micros",
        "synthetic_deep_chain.sqlite.path_max_hops.duration_micros_per_1k_graph_rows",
        "synthetic_deep_chain.sqlite.path_max_hops_256.duration_micros_per_1k_graph_rows",
        "synthetic_deep_chain.sqlite.path_max_hops_512.duration_micros_per_1k_graph_rows",
    ] {
        assert!(
            delta_metrics.contains(metric),
            "metric-digest should compare {metric}: {digest}"
        );
    }
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
        backlog["warnings"]
            .as_array()
            .unwrap()
            .iter()
            .any(|warning| {
                let warning = warning.as_str().unwrap();
                warning.contains("queue-head drift")
                    && warning.contains("agent-doc write --commit")
                    && warning.contains("do not redispatch")
            }),
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
fn graph_db_evidence_falls_back_to_session_line_for_unlinked_backlog() {
    let project = graph_db_project();
    let session = project.path().join("tasks/software/unlinked.md");
    fs::write(
        &session,
        r#"---
agent_doc_session: unlinked
agent_doc_format: template
---

<!-- agent:queue -->
- do [#zqxj]
<!-- /agent:queue -->

## Backlog

<!-- agent:backlog -->
- [ ] [#zqxj] Triage completed queue guidance without code tokens.
<!-- /agent:backlog -->
"#,
    )
    .unwrap();

    let output = run_tsift(vec![
        "index".to_string(),
        project.path().to_string_lossy().to_string(),
    ]);
    assert!(
        output.status.success(),
        "index failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    let evidence = graph_db_json(
        project.path(),
        Backend::Sqlite,
        vec![
            "evidence".to_string(),
            "zqxj".to_string(),
            "--depth".to_string(),
            "3".to_string(),
            "--limit".to_string(),
            "8".to_string(),
        ],
    );

    assert_eq!(evidence["target_node"]["kind"], "backlog", "{evidence}");
    assert!(
        !evidence["worker_context"].as_array().unwrap().is_empty(),
        "{evidence}"
    );
    assert!(
        evidence["worker_results"].as_array().unwrap().is_empty(),
        "backlog prose containing completed must not become worker_result evidence: {evidence}"
    );
    assert!(
        evidence["source_handles"]
            .as_array()
            .unwrap()
            .iter()
            .any(|node| node["properties"]["file"] == "tasks/software/unlinked.md"),
        "{evidence}"
    );
    assert!(
        evidence["shortest_paths"]
            .as_array()
            .unwrap()
            .iter()
            .any(|path| path["kind"] == "source_handle" && !path["path"].is_null()),
        "{evidence}"
    );
    assert!(
        evidence["next_commands"]
            .as_array()
            .unwrap()
            .iter()
            .any(|command| {
                let command = command.as_str().unwrap();
                command.contains("source-read") && command.contains("unlinked.md")
            }),
        "{evidence}"
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
    assert_eq!(report["cross_target_parallel_safe"], true, "{report}");
    assert!(
        report["per_target_fail_closed"]
            .as_array()
            .unwrap()
            .is_empty(),
        "{report}"
    );
    assert_eq!(report["cached_diff"]["mode"], "cached");
    assert_eq!(report["impact"]["mode"], "cached");
    assert_eq!(
        report["inputs"]["preparation_cache"]["version"], "conflict-matrix-prep-v1",
        "{report}"
    );
    assert_eq!(
        report["inputs"]["preparation_cache"]["status"], "computed",
        "{report}"
    );
    let preparation_phases = report["inputs"]["preparation_timings"].as_array().unwrap();
    for phase in [
        "preparation_cache_lookup",
        "session_review_compute",
        "status_index_gate",
        "context_pack_diff",
        "exploration_materialization",
        "graph_orchestration",
        "staged_diff",
        "impact",
    ] {
        assert!(
            preparation_phases
                .iter()
                .any(|entry| entry["name"] == phase),
            "missing preparation phase {phase}: {report}"
        );
    }
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
    assert_eq!(
        report["inputs"]["shared_preparation"]["evidence_packets"],
        json!(1),
        "{report}"
    );
    assert_eq!(
        report["inputs"]["shared_preparation"]["evidence_cache_status"], "computed",
        "{report}"
    );
    assert!(
        report["inputs"]["shared_preparation"]["source_handles"]
            .as_u64()
            .unwrap()
            > 0,
        "{report}"
    );
    assert_eq!(
        report["inputs"]["shared_preparation"]["dispatch_trace_snapshot_nodes"],
        report["inputs"]["shared_preparation"]["graph_nodes"],
        "{report}"
    );
    let status = graph_db_json(project.path(), Backend::Sqlite, vec!["status".to_string()]);
    assert!(
        report["inputs"]["shared_preparation"]["graph_nodes"]
            .as_u64()
            .unwrap()
            < status["counts"]["nodes"].as_u64().unwrap(),
        "conflict-matrix should use a target-scoped graph snapshot: {report}"
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

    let cached_report = assert_tsift_json(vec![
        "conflict-matrix".to_string(),
        "--path".to_string(),
        session.to_string_lossy().to_string(),
        "--json".to_string(),
        "gval".to_string(),
    ]);
    assert_eq!(
        cached_report["inputs"]["preparation_cache"]["status"], "disk_hit",
        "{cached_report}"
    );
    assert_eq!(
        cached_report["inputs"]["shared_preparation"]["evidence_cache_status"], "disk_hit",
        "{cached_report}"
    );
    let cached_preparation_phases = cached_report["inputs"]["preparation_timings"]
        .as_array()
        .unwrap();
    for phase in [
        "session_review_compute",
        "status_index_gate",
        "context_pack_diff",
        "impact",
    ] {
        let timing = cached_preparation_phases
            .iter()
            .find(|entry| entry["name"] == phase)
            .unwrap_or_else(|| panic!("missing cached preparation phase {phase}: {cached_report}"));
        assert_eq!(
            timing["duration_micros"], 0,
            "cache hit should skip {phase}: {cached_report}"
        );
        assert!(
            timing["detail"]
                .as_str()
                .unwrap()
                .contains("source/document/staged-diff watermark"),
            "cache hit should keep the freshness guard in {phase}: {cached_report}"
        );
    }

    let directory_report = assert_tsift_json(vec![
        "conflict-matrix".to_string(),
        "--path".to_string(),
        project.path().to_string_lossy().to_string(),
        "--json".to_string(),
        "gval".to_string(),
    ]);
    assert_eq!(directory_report["targets"], json!(["gval"]));
    assert!(
        directory_report["inputs"]["preparation_cache"]["document_watermark"]
            .as_str()
            .unwrap()
            .len()
            > 8,
        "{directory_report}"
    );
}

#[test]
fn conflict_matrix_separates_per_target_fail_closed_from_cross_target_overlap() {
    let project = graph_db_project();
    init_git_repo(project.path());
    let session = project.path().join("tasks/software/tsift.md");
    let projection_id = "projection:tsift-traversal:root".to_string();

    let report = assert_tsift_json(vec![
        "conflict-matrix".to_string(),
        "--path".to_string(),
        session.to_string_lossy().to_string(),
        "--json".to_string(),
        projection_id.clone(),
    ]);

    assert_eq!(report["cross_target_parallel_safe"], true, "{report}");
    assert_eq!(report["fail_closed"], true, "{report}");
    assert_eq!(report["can_parallel"], false, "{report}");
    let per_target = report["per_target_fail_closed"].as_array().unwrap();
    assert_eq!(per_target.len(), 1, "{report}");
    assert!(
        per_target
            .iter()
            .any(|entry| entry["target"] == projection_id),
        "{report}"
    );
    assert!(
        report["conflicts"].as_array().unwrap().is_empty(),
        "{report}"
    );
}

#[test]
fn conflict_matrix_downgrades_completed_worker_result_without_source_handles() {
    let project = graph_db_project();
    init_git_repo(project.path());
    let session = project.path().join("tasks/software/tsift.md");

    let report = assert_tsift_json(vec![
        "conflict-matrix".to_string(),
        "--path".to_string(),
        session.to_string_lossy().to_string(),
        "--depth".to_string(),
        "1".to_string(),
        "--json".to_string(),
        "gval".to_string(),
    ]);

    assert_eq!(report["fail_closed"], false, "{report}");
    assert_eq!(report["can_parallel"], true, "{report}");
    assert!(
        report["per_target_fail_closed"]
            .as_array()
            .unwrap()
            .is_empty(),
        "{report}"
    );

    let candidate = &report["candidates"].as_array().unwrap()[0];
    assert_eq!(candidate["target"], "gval", "{report}");
    assert_eq!(candidate["risk"], "low", "{report}");
    assert_eq!(candidate["previously_completed"], true, "{report}");
    assert!(
        candidate["owned_files"].as_array().unwrap().is_empty(),
        "{report}"
    );
    assert!(
        candidate["source_handles"].as_array().unwrap().is_empty(),
        "{report}"
    );
    assert_eq!(candidate["worker_feedback"]["completed"], 1, "{report}");
    assert!(
        candidate["risk_reasons"].as_array().unwrap().is_empty(),
        "completed worker_result should downgrade no-owned-files to info, not a hard risk reason: {report}"
    );
    assert!(
        report["warnings"]
            .as_array()
            .unwrap()
            .iter()
            .any(|warning| warning.as_str().unwrap().contains("previously completed")),
        "{report}"
    );
    assert!(
        report["worker_prompt_packets"]
            .as_array()
            .unwrap()
            .iter()
            .any(|packet| {
                packet["target"] == "gval"
                    && packet["previously_completed"] == true
                    && packet["risk"] == "low"
            }),
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
    assert!(
        packets.iter().any(|packet| {
            packet["blocks"].as_array().unwrap().is_empty()
                != packet["blocked_by"].as_array().unwrap().is_empty()
        }),
        "shared file conflicts should emit explicit scheduler block edges: {report}"
    );
    assert!(
        packets
            .iter()
            .any(|packet| packet["parallel_safe"] == false),
        "unsafe shared ownership should mark affected packets not parallel_safe: {report}"
    );
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
        solo_packet["required_context"]["source_handles"]
            .as_array()
            .unwrap()
            .iter()
            .any(|handle| !handle.as_str().unwrap().is_empty()),
        "{report}"
    );
    assert!(
        solo_packet["graph_handles"]["target_node_id"]
            .as_str()
            .unwrap()
            .starts_with("gbak-"),
        "{report}"
    );
    assert!(
        solo_packet["graph_handles"]["evidence_packet_id"]
            .as_str()
            .unwrap()
            .starts_with("gevd-"),
        "{report}"
    );
    assert_eq!(
        solo_packet["graph_handles"]["worker_prompt_packet_id"], solo_packet["packet_id"],
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

    let dependency_dag = assert_tsift_json(vec![
        "dependency-dag".to_string(),
        "--path".to_string(),
        session.to_string_lossy().to_string(),
        "--json".to_string(),
        "gval".to_string(),
        "solo".to_string(),
        "shrd".to_string(),
    ]);
    assert_contract_fields(&fixture, "dependency_dag", &dependency_dag);
    assert_eq!(
        dependency_dag["projection_freshness"]["status"], "current",
        "{dependency_dag}"
    );
    assert!(
        !dependency_dag["projection_hashes"]
            .as_array()
            .unwrap()
            .is_empty(),
        "{dependency_dag}"
    );
    assert!(
        dependency_dag["edges"]
            .as_array()
            .unwrap()
            .iter()
            .any(|edge| edge["kind"] == "worker_result_follow_up"),
        "{dependency_dag}"
    );
    assert!(
        dependency_dag["topo_batches"].is_array(),
        "{dependency_dag}"
    );

    let cyclic_dependency_dag = assert_tsift_json(vec![
        "dependency-dag".to_string(),
        "--path".to_string(),
        session.to_string_lossy().to_string(),
        "--json".to_string(),
        "gval".to_string(),
        "solo".to_string(),
    ]);
    assert_eq!(
        cyclic_dependency_dag["cycle_diagnostics"]["has_cycles"], true,
        "{cyclic_dependency_dag}"
    );
    assert!(
        cyclic_dependency_dag["cycle_diagnostics"]["cycle_edges"]
            .as_array()
            .unwrap()
            .iter()
            .any(|edge| edge["kind"] == "worker_result_follow_up"),
        "{cyclic_dependency_dag}"
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
            .any(|command| command
                .as_str()
                .unwrap()
                .contains("graph-db --path . status")),
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
                .contains("graph-db --path . doctor")),
        "{fixture}"
    );
    assert!(
        fixture["command_sequence"]
            .as_array()
            .unwrap()
            .iter()
            .any(|command| command.as_str().unwrap().contains("dependency-dag")),
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
        fixture["command_sequence"]
            .as_array()
            .unwrap()
            .iter()
            .any(|command| {
                let command = command.as_str().unwrap();
                command.contains("convex-snapshot")
                    && command.contains("stale-convex-snapshot.json")
                    && command.contains("drift")
            }),
        "{fixture}"
    );
    assert!(
        fixture["command_sequence"]
            .as_array()
            .unwrap()
            .iter()
            .any(|command| {
                let command = command.as_str().unwrap();
                command.contains("convex-sync")
                    && command.contains("stale-convex-snapshot.json")
                    && command.contains("--chunk-size 25")
            }),
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
        fixture["expected_contracts"]["contract_versions"]
            .as_array()
            .unwrap()
            .iter()
            .any(|version| version == "dependency-dag-v1"),
        "{fixture}"
    );
    assert_eq!(
        fixture["expected_contracts"]["scheduler_fields"]["parallel_safe"],
        "per packet/candidate boolean; false when serial block edges or fail-closed ownership are present",
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
    assert!(
        fixture["required_trace_links"]
            .as_array()
            .unwrap()
            .iter()
            .any(|link| link
                .as_str()
                .unwrap()
                .contains("dependency_dag.topo_batches")),
        "{fixture}"
    );
    assert!(
        fixture["expected_contracts"]["operator_replay"]["stale_convex_repair_rule"]
            .as_str()
            .unwrap()
            .contains("convex-sync"),
        "{fixture}"
    );
}

#[test]
fn agent_orchestration_acceptance_pack_regenerates_from_real_queue_fixture() {
    let project = graph_db_project();
    init_git_repo(project.path());
    let session = project.path().join("tasks/software/tsift.md");
    let fixture = agent_orchestration_acceptance_pack_fixture();
    let actual = regenerate_agent_orchestration_acceptance_samples(project.path(), &session);
    let Some(expected) = fixture.get("regenerated_samples") else {
        panic!(
            "agent-orchestration acceptance fixture is missing regenerated_samples:\n{}",
            serde_json::to_string_pretty(&actual).unwrap()
        );
    };
    assert_eq!(
        expected, &actual,
        "agent-orchestration acceptance fixture drifted; update fixtures/graph-db-operator-examples/agent-orchestration-acceptance-pack.json regenerated_samples from the live queue fixture output"
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
    assert_eq!(
        report["shared_preparation"]["evidence_packets"]
            .as_u64()
            .unwrap(),
        report["evidence_packet_ids"].as_array().unwrap().len() as u64,
        "{report}"
    );
    assert!(
        report["shared_preparation"]["dispatch_trace_snapshot_edges"]
            .as_u64()
            .unwrap()
            > 0,
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
    assert!(
        report["worker_prompt_packets"]
            .as_array()
            .unwrap()
            .iter()
            .all(|packet| {
                packet["required_context"]["source_handles"].is_array()
                    && packet["graph_handles"]["evidence_packet_id"]
                        .as_str()
                        .unwrap()
                        .starts_with("gevd-")
                    && packet["graph_handles"]["worker_prompt_packet_id"] == packet["packet_id"]
            }),
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
    assert!(html.contains("parallel_safe"), "{html}");
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
    let trace_evidence_packet_ids = trace["evidence_packet_ids"].as_array().unwrap();
    assert!(
        !trace_evidence_packet_ids.is_empty(),
        "dispatch-trace should expose the evidence packet ids it shares with conflict-matrix: {trace}"
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
    assert!(
        trace_evidence_packet_ids
            .iter()
            .filter_map(|packet| packet.as_str())
            .any(|packet| html.contains(packet)),
        "{html}"
    );
    assert!(html.contains("Follow-up debt"), "{html}");
    assert!(html.contains("closure"), "{html}");
}

#[test]
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
    graph_db_json(project.path(), Backend::Sqlite, vec!["refresh".to_string()]);

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
    graph_db_json(project.path(), Backend::Sqlite, vec!["refresh".to_string()]);
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

    let stderr = graph_db_failure(project.path(), Backend::Sqlite, vec!["refresh".to_string()]);
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
            .all(|check| check["status"] == "ok" || check["status"] == "not_needed"),
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
            .all(|check| check["status"] == "ok" || check["status"] == "not_needed"),
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

fn query_text_column(db_path: &Path, sql: &str) -> Vec<String> {
    let conn = Connection::open(db_path).unwrap();
    let mut stmt = conn.prepare(sql).unwrap();
    stmt.query_map([], |row| row.get::<_, String>(0))
        .unwrap()
        .collect::<Result<Vec<_>, _>>()
        .unwrap()
}

fn sql_literal(value: &str) -> String {
    format!("'{}'", value.replace('\'', "''"))
}

#[test]
fn graph_db_scale_caps_pagination_paths_doctor_and_sqlite_plans() {
    let project = large_graph_db_project(120);
    graph_db_json(project.path(), Backend::Sqlite, vec!["refresh".to_string()]);
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
    assert_sqlite_page_uses_index(&neighborhood["page"], "idx_graph_edges_from_kind");
    assert_sqlite_page_diagnostic_contains(
        &neighborhood["page"],
        "one recursive reachable-set CTE",
    );

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
            "SELECT to_id FROM graph_edges INDEXED BY idx_graph_edges_from_kind WHERE from_id = {} AND kind = 'calls' ORDER BY to_id, kind",
            sql_literal(&first_id)
        ),
    )
    .join("\n");
    assert!(
        edge_plan.contains("idx_graph_edges_from_kind"),
        "expected graph_edges from/kind index in plan:\n{edge_plan}"
    );

    let incident = graph_db_json(
        project.path(),
        Backend::Sqlite,
        vec![
            "incident".to_string(),
            first_id.clone(),
            "--edge-kind".to_string(),
            "calls".to_string(),
            "--property".to_string(),
            "weight=1".to_string(),
            "--limit".to_string(),
            "10".to_string(),
        ],
    );
    assert_eq!(incident["page"]["truncated"], false, "{incident}");
    assert_sqlite_page_uses_index(&incident["page"], "idx_graph_edges_from_kind");
    assert_sqlite_page_uses_index(&incident["page"], "idx_graph_edges_to_kind");
    assert_sqlite_page_uses_index(
        &incident["page"],
        "idx_graph_edge_properties_key_value_edge",
    );
    assert!(
        incident["page"]["diagnostics"]
            .as_array()
            .unwrap()
            .iter()
            .any(|diagnostic| diagnostic
                .as_str()
                .unwrap()
                .contains("UNION over from_id/to_id")),
        "{incident}"
    );

    let escaped_first_id = sql_literal(&first_id);
    let union_incident_sql = format!(
        "SELECT edge_key FROM (
            SELECT edge_key FROM graph_edges INDEXED BY idx_graph_edges_from_kind
            WHERE from_id = {escaped_first_id} AND kind = 'calls'
            UNION
            SELECT edge_key FROM graph_edges INDEXED BY idx_graph_edges_to_kind
            WHERE to_id = {escaped_first_id} AND kind = 'calls'
        ) ORDER BY edge_key"
    );
    let or_incident_sql = format!(
        "SELECT edge_key FROM graph_edges
         WHERE (from_id = {escaped_first_id} OR to_id = {escaped_first_id})
           AND kind = 'calls'
         ORDER BY edge_key"
    );
    assert_eq!(
        query_text_column(&db_path, &union_incident_sql),
        query_text_column(&db_path, &or_incident_sql),
        "UNION incident scan must preserve current OR predicate semantics"
    );
    let incident_plan = query_plan_details(&db_path, &union_incident_sql).join("\n");
    assert!(
        incident_plan.contains("idx_graph_edges_from_kind")
            && incident_plan.contains("idx_graph_edges_to_kind"),
        "expected incident UNION scan to use both directional indexes:\n{incident_plan}"
    );

    let first_edge_id = incident["edges"][0]["id"].as_str().unwrap();
    let edge_lookup_plan = query_plan_details(
        &db_path,
        &format!(
            "SELECT edge_key FROM graph_edges INDEXED BY idx_graph_edges_edge_key WHERE edge_key = {}",
            sql_literal(first_edge_id)
        ),
    )
    .join("\n");
    assert!(
        edge_lookup_plan.contains("idx_graph_edges_edge_key"),
        "expected edge lookup index in plan:\n{edge_lookup_plan}"
    );

    let edge_property_scan = graph_db_json(
        project.path(),
        Backend::Sqlite,
        vec![
            "edges".to_string(),
            "--edge-kind".to_string(),
            "calls".to_string(),
            "--property".to_string(),
            "weight=1".to_string(),
            "--limit".to_string(),
            "10".to_string(),
        ],
    );
    assert_sqlite_page_uses_index(
        &edge_property_scan["page"],
        "idx_graph_edge_properties_key_value_edge",
    );
    assert_sqlite_page_uses_index(&edge_property_scan["page"], "idx_graph_edges_edge_key");
    assert_sqlite_page_diagnostic_contains(
        &edge_property_scan["page"],
        "edge property primary filter matched",
    );
    assert_sqlite_page_diagnostic_contains(
        &edge_property_scan["page"],
        "drives from SQLite materialized property rows",
    );
}

#[test]
fn graph_db_evidence_stays_bounded_on_large_session_backlog_fixture() {
    let project = large_graph_db_project(240);
    let started = Instant::now();
    let evidence = graph_db_json(
        project.path(),
        Backend::Sqlite,
        vec![
            "evidence".to_string(),
            "b000".to_string(),
            "--depth".to_string(),
            "3".to_string(),
            "--limit".to_string(),
            "8".to_string(),
        ],
    );

    assert_eq!(evidence["target_node"]["kind"], "backlog", "{evidence}");
    assert!(
        evidence["worker_context"].as_array().unwrap().len() <= 8,
        "{evidence}"
    );
    assert!(
        evidence["source_handles"].as_array().unwrap().len() <= 8,
        "{evidence}"
    );
    assert!(
        evidence["shortest_paths"].as_array().unwrap().len() <= 16,
        "{evidence}"
    );
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "graph-db evidence should stay bounded on large session fixtures"
    );
}

#[test]
fn graph_db_evidence_refreshes_only_hinted_agent_doc_session() {
    let project = large_graph_db_project(80);
    let noise_dir = project.path().join("tasks/noise");
    fs::create_dir_all(&noise_dir).unwrap();
    for idx in 0..120 {
        fs::write(
            noise_dir.join(format!("noise-{idx:03}.md")),
            format!(
                "<!-- agent:backlog -->\n- [ ] [#n{idx:03}] Background noise backlog item.\n<!-- /agent:backlog -->\n"
            ),
        )
        .unwrap();
    }
    let session = project.path().join("tasks/software/tsift.md");
    let started = Instant::now();
    let evidence = assert_tsift_json(vec![
        "graph-db".to_string(),
        "--path".to_string(),
        session.to_string_lossy().to_string(),
        "--json".to_string(),
        "evidence".to_string(),
        "b000".to_string(),
        "--depth".to_string(),
        "3".to_string(),
        "--limit".to_string(),
        "8".to_string(),
    ]);

    assert_eq!(evidence["target_node"]["kind"], "backlog", "{evidence}");
    assert_eq!(
        evidence["target_node"]["properties"]["path"], "tasks/software/tsift.md",
        "{evidence}"
    );
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "session-hinted graph-db evidence should not walk unrelated markdown"
    );
}

#[test]
fn graph_db_evidence_prefers_hinted_session_path_for_colliding_backlog_ids() {
    let project = large_graph_db_project(80);
    let other_dir = project.path().join("tasks/professional");
    fs::create_dir_all(&other_dir).unwrap();
    fs::write(
        other_dir.join("equityfundingsource.md"),
        "<!-- agent:backlog -->\n- [ ] [#b000] Unrelated external backlog row.\n<!-- /agent:backlog -->\n",
    )
    .unwrap();

    graph_db_json(project.path(), Backend::Sqlite, vec!["refresh".to_string()]);
    let ambiguous = graph_db_failure(
        project.path(),
        Backend::Sqlite,
        vec![
            "evidence".to_string(),
            "b000".to_string(),
            "--depth".to_string(),
            "3".to_string(),
            "--limit".to_string(),
            "8".to_string(),
        ],
    );
    assert!(ambiguous.contains("ambiguous"), "{ambiguous}");
    assert!(ambiguous.contains("tasks/software/tsift.md"), "{ambiguous}");
    assert!(
        ambiguous.contains("tasks/professional/equityfundingsource.md"),
        "{ambiguous}"
    );

    let session = project.path().join("tasks/software/tsift.md");
    let evidence = graph_db_json(
        &session,
        Backend::Sqlite,
        vec![
            "evidence".to_string(),
            "b000".to_string(),
            "--depth".to_string(),
            "3".to_string(),
            "--limit".to_string(),
            "8".to_string(),
        ],
    );
    assert_eq!(evidence["target_node"]["kind"], "backlog", "{evidence}");
    assert_eq!(
        evidence["target_node"]["properties"]["path"], "tasks/software/tsift.md",
        "{evidence}"
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

// -- #convexsnapshotscale: paginated remote-snapshot transport coverage ---------
//
// The live acceptance test above (`live_convex_graph_backend_acceptance_...`)
// only runs against a real Convex deployment behind an env gate. We also want
// a fully hermetic integration test that exercises the paginated transport
// (`snapshot_meta` + `snapshot_nodes_page` + `snapshot_edges_page`) without
// requiring Docker or a live backend. The mock server below is the minimum
// stdlib HTTP/1.1 implementation needed for ureq to talk to it.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpListener;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;

struct MockConvexSnapshotServer {
    addr: String,
    shutdown: Arc<AtomicBool>,
    handle: Option<thread::JoinHandle<()>>,
    pub call_log: Arc<std::sync::Mutex<Vec<String>>>,
}

impl MockConvexSnapshotServer {
    fn start(snapshot: Value, page_size: usize) -> Self {
        Self::start_with_projection_hash(snapshot, page_size, false)
    }

    fn start_with_projection_hash(
        snapshot: Value,
        page_size: usize,
        include_projection_hash: bool,
    ) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind mock server");
        listener
            .set_nonblocking(true)
            .expect("set nonblocking on listener");
        let addr = listener.local_addr().unwrap().to_string();
        let shutdown = Arc::new(AtomicBool::new(false));
        let call_log: Arc<std::sync::Mutex<Vec<String>>> =
            Arc::new(std::sync::Mutex::new(Vec::new()));
        let shutdown_clone = Arc::clone(&shutdown);
        let call_log_clone = Arc::clone(&call_log);
        let snapshot = Arc::new(snapshot);
        let handle = thread::spawn(move || {
            while !shutdown_clone.load(Ordering::SeqCst) {
                match listener.accept() {
                    Ok((mut stream, _)) => {
                        stream
                            .set_nonblocking(false)
                            .expect("clear nonblocking on accepted stream");
                        let mut reader =
                            BufReader::new(stream.try_clone().expect("clone stream for read half"));
                        let mut request_line = String::new();
                        if reader.read_line(&mut request_line).is_err() {
                            continue;
                        }
                        let mut content_length = 0usize;
                        loop {
                            let mut line = String::new();
                            if reader.read_line(&mut line).is_err() {
                                break;
                            }
                            if line == "\r\n" || line.is_empty() {
                                break;
                            }
                            if let Some(rest) = line
                                .strip_prefix("Content-Length:")
                                .or_else(|| line.strip_prefix("content-length:"))
                            {
                                content_length = rest.trim().parse().unwrap_or(0);
                            }
                        }
                        let mut body_bytes = vec![0u8; content_length];
                        if content_length > 0 {
                            reader
                                .read_exact(&mut body_bytes)
                                .expect("read request body");
                        }
                        let body: Value =
                            serde_json::from_slice(&body_bytes).unwrap_or(Value::Null);
                        let op = body["operation"].as_str().unwrap_or("").to_string();
                        call_log_clone.lock().unwrap().push(op.clone());
                        let response_body = handle_mock_request(
                            &op,
                            &body,
                            &snapshot,
                            page_size,
                            include_projection_hash,
                        );
                        let body_str = serde_json::to_string(&response_body).unwrap();
                        let response = format!(
                            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                            body_str.len(),
                            body_str
                        );
                        let _ = stream.write_all(response.as_bytes());
                        let _ = stream.flush();
                    }
                    Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(10));
                    }
                    Err(_) => break,
                }
            }
        });
        Self {
            addr,
            shutdown,
            handle: Some(handle),
            call_log,
        }
    }

    fn endpoint(&self) -> String {
        format!("http://{}/tsift/graph", self.addr)
    }

    fn calls(&self) -> Vec<String> {
        self.call_log.lock().unwrap().clone()
    }
}

impl Drop for MockConvexSnapshotServer {
    fn drop(&mut self) {
        self.shutdown.store(true, Ordering::SeqCst);
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

fn handle_mock_request(
    op: &str,
    body: &Value,
    snapshot: &Value,
    page_size: usize,
    include_projection_hash: bool,
) -> Value {
    match op {
        "snapshot_meta" => {
            let mut response = json!({
                "status": "ok",
                "meta": {
                    "indexes": required_convex_indexes_json(),
                    "pageSize": page_size,
                }
            });
            if include_projection_hash
                && let Some(meta_id) = body["projectionMetaId"].as_str()
                && let Some(hash) = projection_hash_from_snapshot(snapshot, meta_id)
            {
                response["meta"]["projectionHash"] = json!(hash);
            }
            response
        }
        "snapshot_nodes_page" => page_response(snapshot, "nodes", "externalId", body, page_size),
        "snapshot_edges_page" => page_response(snapshot, "edges", "edgeKey", body, page_size),
        other => json!({
            "status": "error",
            "message": format!("mock server: unknown operation: {other}"),
        }),
    }
}

fn projection_hash_from_snapshot(snapshot: &Value, meta_id: &str) -> Option<String> {
    snapshot["nodes"]
        .as_array()?
        .iter()
        .find(|node| node["externalId"].as_str() == Some(meta_id))
        .and_then(|node| node["properties"]["content_hash"].as_str())
        .map(str::to_string)
}

fn page_response(
    snapshot: &Value,
    table: &str,
    cursor_field: &str,
    body: &Value,
    page_size: usize,
) -> Value {
    let mut rows: Vec<Value> = snapshot[table].as_array().cloned().unwrap_or_default();
    // Mock backend sorts by the cursor field (matches the schema's
    // by_external_id / by_edge_key index ordering).
    rows.sort_by(|a, b| {
        a[cursor_field]
            .as_str()
            .unwrap_or("")
            .cmp(b[cursor_field].as_str().unwrap_or(""))
    });
    let cursor = body["cursor"].as_str().map(|s| s.to_string());
    let start = match &cursor {
        Some(c) => rows
            .iter()
            .position(|row| row[cursor_field].as_str().unwrap_or("") > c.as_str())
            .unwrap_or(rows.len()),
        None => 0,
    };
    let end = (start + page_size).min(rows.len());
    let page_rows: Vec<Value> = rows[start..end].to_vec();
    let next_cursor: Value = if end < rows.len() && page_rows.len() == page_size {
        Value::String(
            page_rows
                .last()
                .and_then(|r| r[cursor_field].as_str())
                .unwrap_or("")
                .to_string(),
        )
    } else {
        Value::Null
    };
    json!({
        "status": "ok",
        "page": {
            "rows": page_rows,
            "nextCursor": next_cursor,
        }
    })
}

#[test]
fn convex_sync_remote_snapshot_uses_paginated_transport_against_mock_backend() {
    // Build the local projection snapshot, then serve those rows from a mock
    // server. With the projection identical on both sides, freshness must come
    // back as `current` (not `unchecked` or `stale`).
    let project = graph_db_project();
    let snapshot = current_convex_snapshot(project.path());
    let node_count = snapshot["nodes"].as_array().unwrap().len();
    let edge_count = snapshot["edges"].as_array().unwrap().len();
    assert!(
        node_count > 0 && edge_count > 0,
        "expected non-empty projection: {snapshot}"
    );

    // Small page size forces multiple pages even on the small synthetic graph
    // — exercises the cursor loop, not just the first-page short-circuit.
    let server = MockConvexSnapshotServer::start(snapshot.clone(), 3);

    let endpoint = server.endpoint();
    let output = run_tsift(vec![
        "convex-sync".to_string(),
        project.path().to_string_lossy().to_string(),
        "--remote-snapshot".to_string(),
        "--endpoint".to_string(),
        endpoint.clone(),
        "--json".to_string(),
    ]);
    assert!(
        output.status.success(),
        "convex-sync --remote-snapshot failed against mock\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let report: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(
        report["freshness"]["status"], "current",
        "expected current freshness against identical mock snapshot, got: {}",
        report["freshness"]
    );
    assert!(
        report["freshness"]["snapshot_hash"].is_string(),
        "expected snapshot_hash populated: {}",
        report["freshness"]
    );
    assert_eq!(
        report["freshness"]["local_hash"], report["freshness"]["snapshot_hash"],
        "expected local_hash == snapshot_hash: {}",
        report["freshness"]
    );

    let calls = server.calls();
    assert!(
        calls.first().is_some_and(|op| op == "snapshot_meta"),
        "expected first call to be snapshot_meta, got {calls:?}"
    );
    let nodes_pages = calls
        .iter()
        .filter(|op| *op == "snapshot_nodes_page")
        .count();
    let edges_pages = calls
        .iter()
        .filter(|op| *op == "snapshot_edges_page")
        .count();
    assert!(
        nodes_pages >= node_count.div_ceil(3),
        "expected at least {} node page calls, got {nodes_pages} (calls={calls:?})",
        node_count.div_ceil(3)
    );
    assert!(
        edges_pages >= edge_count.div_ceil(3),
        "expected at least {} edge page calls, got {edges_pages} (calls={calls:?})",
        edge_count.div_ceil(3)
    );
    assert!(
        !calls.iter().any(|op| op == "snapshot"),
        "must not fall back to legacy single-shot snapshot when paginated path works: {calls:?}"
    );
}

#[test]
fn convex_sync_remote_snapshot_uses_projection_hash_shortcut_when_current() {
    let project = graph_db_project();
    let snapshot = current_convex_snapshot(project.path());
    let server = MockConvexSnapshotServer::start_with_projection_hash(snapshot, 3, true);

    let output = run_tsift(vec![
        "convex-sync".to_string(),
        project.path().to_string_lossy().to_string(),
        "--remote-snapshot".to_string(),
        "--endpoint".to_string(),
        server.endpoint(),
        "--json".to_string(),
    ]);
    assert!(
        output.status.success(),
        "convex-sync --remote-snapshot failed against hash shortcut mock\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let report: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(report["freshness"]["status"], "current", "{report}");
    assert_eq!(
        report["node_upserts"].as_array().unwrap().len(),
        0,
        "{report}"
    );
    assert_eq!(
        report["edge_upserts"].as_array().unwrap().len(),
        0,
        "{report}"
    );
    assert!(
        report["diagnostics"]
            .as_array()
            .unwrap()
            .iter()
            .any(|entry| entry
                .as_str()
                .is_some_and(|text| text.contains("projection hash matched"))),
        "expected hash shortcut diagnostic: {report}"
    );

    let calls = server.calls();
    assert_eq!(calls, vec!["snapshot_meta"], "unexpected calls: {calls:?}");
}

// --- #trt1p3: graph-db map findings annotation + md/html projection ----------

fn add_finding(project: &Path, kind: &str, title: &str, about: &str, status: &str) {
    let output = run_tsift(vec![
        "finding".to_string(),
        "add".to_string(),
        "--path".to_string(),
        project.to_string_lossy().to_string(),
        "--kind".to_string(),
        kind.to_string(),
        "--title".to_string(),
        title.to_string(),
        "--body".to_string(),
        "the why".to_string(),
        "--about".to_string(),
        about.to_string(),
        "--status".to_string(),
        status.to_string(),
    ]);
    assert!(
        output.status.success(),
        "finding add failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

fn map_projection(project: &Path, format: &str) -> String {
    let output = run_tsift(vec![
        "graph-db".to_string(),
        "--path".to_string(),
        project.to_string_lossy().to_string(),
        "map".to_string(),
        "--format".to_string(),
        format.to_string(),
    ]);
    assert!(
        output.status.success(),
        "graph-db map --format {format} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).into_owned()
}

/// A hub whose symbol carries a trusted, fresh finding is annotated in the map
/// JSON overview (#trt1p3 graph menu).
#[test]
fn graph_db_map_annotates_hub_with_trusted_finding() {
    let project = graph_db_project();
    graph_db_json(project.path(), Backend::Sqlite, vec!["refresh".to_string()]);
    add_finding(
        project.path(),
        "decision",
        "alpha is the orchestrator",
        "alpha",
        "trusted",
    );

    let map = graph_db_json(project.path(), Backend::Sqlite, vec!["map".to_string()]);
    let annotated = map["overview"]["top_hubs"]
        .as_array()
        .unwrap()
        .iter()
        .any(|hub| {
            hub["label"] == "alpha"
                && hub["findings"]
                    .as_array()
                    .map(|f| f.iter().any(|x| x["title"] == "alpha is the orchestrator"))
                    .unwrap_or(false)
        });
    assert!(
        annotated,
        "expected alpha hub annotated with the trusted finding, got {}",
        map["overview"]["top_hubs"]
    );
}

/// The md and html projections render attached findings (#trt1p3 exports).
#[test]
fn graph_db_map_md_and_html_projections_render_findings() {
    let project = graph_db_project();
    graph_db_json(project.path(), Backend::Sqlite, vec!["refresh".to_string()]);
    add_finding(
        project.path(),
        "decision",
        "alpha is the orchestrator",
        "alpha",
        "trusted",
    );

    let md = map_projection(project.path(), "md");
    assert!(
        md.starts_with("# Graph Map"),
        "md should be markdown: {md:.120}"
    );
    assert!(
        md.contains("📌 decision: alpha is the orchestrator (about `alpha`)"),
        "md should render the finding line: {md}"
    );

    let html = map_projection(project.path(), "html");
    assert!(html.starts_with("<!DOCTYPE html>"), "html should be a page");
    assert!(
        html.contains("class=\"finding\">📌 decision: alpha is the orchestrator"),
        "html should render the finding: {html}"
    );
}

/// Draft and stale findings never annotate the map — same trusted/fresh guard
/// as Phase 2 hot-path injection (#trt1p2).
#[test]
fn graph_db_map_excludes_draft_findings() {
    let project = graph_db_project();
    graph_db_json(project.path(), Backend::Sqlite, vec!["refresh".to_string()]);
    add_finding(
        project.path(),
        "note",
        "draft about alpha",
        "alpha",
        "draft",
    );

    let map = graph_db_json(project.path(), Backend::Sqlite, vec!["map".to_string()]);
    let leaked = map["overview"]["top_hubs"]
        .as_array()
        .unwrap()
        .iter()
        .any(|hub| {
            hub["findings"]
                .as_array()
                .map(|f| f.iter().any(|x| x["title"] == "draft about alpha"))
                .unwrap_or(false)
        });
    assert!(
        !leaked,
        "draft finding must not annotate the map: {}",
        map["overview"]["top_hubs"]
    );
}
