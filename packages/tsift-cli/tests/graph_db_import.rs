//! `tsift graph-db import` (#sdktsiftedges): external edge files join the
//! indexed graph, survive refresh, and re-import idempotently.

use serde_json::{Value, json};
use std::fs;
use std::path::Path;
use std::process::Command;

fn tsift(args: &[&str]) -> Value {
    let output = Command::new(env!("CARGO_BIN_EXE_tsift-cli"))
        .args(args)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "tsift {} failed\nstdout:\n{}\nstderr:\n{}",
        args.join(" "),
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).unwrap_or_else(|err| {
        panic!(
            "tsift {} printed non-JSON ({err}):\n{}",
            args.join(" "),
            String::from_utf8_lossy(&output.stdout)
        )
    })
}

fn tsift_fails(args: &[&str]) -> String {
    let output = Command::new(env!("CARGO_BIN_EXE_tsift-cli"))
        .args(args)
        .output()
        .unwrap();
    assert!(
        !output.status.success(),
        "tsift {} unexpectedly succeeded",
        args.join(" ")
    );
    String::from_utf8_lossy(&output.stderr).into_owned()
}

/// A miniature haiven-sdk: one JSON Schema contract, its TypeScript
/// declaration, the WebSocket handler that carries it, and a codegen trace
/// shaped like haiven-sdk `codegen/trace.json` (+ `model.json`).
fn sdk_project() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    fs::create_dir_all(root.join("contracts")).unwrap();
    fs::create_dir_all(root.join("sdk")).unwrap();
    fs::create_dir_all(root.join("codegen")).unwrap();
    fs::write(
        root.join("contracts/chat-message.json"),
        r#"{
  "$schema": "https://json-schema.org/draft/2020-12/schema",
  "title": "ChatMessageV0",
  "type": "object",
  "properties": {
    "body": { "type": "string" }
  }
}
"#,
    )
    .unwrap();
    fs::write(
        root.join("sdk/contracts.ts"),
        "// generated\n\nexport interface ChatMessageV0 {\n\tbody: string;\n}\n",
    )
    .unwrap();
    fs::write(
        root.join("sdk/ws.ts"),
        "// generated\n\nexport function onChatMessage(handler: () => void): void {\n\thandler();\n}\n",
    )
    .unwrap();
    write_trace(root, true);
    fs::write(
        root.join("codegen/model.json"),
        serde_json::to_string_pretty(&json!({
            "vocabularies": {},
            "contracts": {},
            "rest": {"components": {}, "operations": {}},
            "ws": {"requests": [], "events": {"chat.message": "ChatMessageV0"}}
        }))
        .unwrap(),
    )
    .unwrap();
    let root_arg = root.to_string_lossy().to_string();
    tsift(&["index", &root_arg, "--json"]);
    dir
}

fn write_trace(root: &Path, with_scenario: bool) {
    let mut event = json!({
        "declared": {
            "typescript": "sdk/ws.ts:3",
            "cpp": "sdks/unreal/HaivenWsApi.h:39"
        }
    });
    if with_scenario {
        event["conformance"] = json!(["gaming/v0#an-event-carries-no-correlation"]);
    }
    let trace = json!({
        "vocabularies": {},
        "contracts": {
            "ChatMessageV0": {
                "declared": {
                    "typescript": "sdk/contracts.ts:3",
                    "gdscript": "sdks/godot/haiven_contracts.gd:5"
                }
            }
        },
        "rest": {},
        "ws": {"requests": {}, "events": {"chat.message": event}},
        "conformance": {"suites": ["gaming/v0"], "operations": 1, "exercised": 1}
    });
    fs::write(
        root.join("codegen/trace.json"),
        serde_json::to_string_pretty(&trace).unwrap(),
    )
    .unwrap();
}

fn report(value: &Value) -> &Value {
    value.get("report").unwrap_or(value)
}

fn contract_node_id(root: &str) -> String {
    let symbols = tsift(&[
        "graph-db",
        "--path",
        root,
        "kind",
        "symbol",
        "--property",
        "ref_id=ChatMessageV0",
        "--json",
    ]);
    report(&symbols)["nodes"]
        .as_array()
        .unwrap()
        .iter()
        .find(|node| {
            node["properties"]["path"] == "contracts/chat-message.json"
                && node["properties"]["detail"]
                    .as_str()
                    .is_some_and(|detail| detail.starts_with("json"))
        })
        .unwrap_or_else(|| panic!("indexed JSON Schema contract node missing: {symbols}"))["id"]
        .as_str()
        .unwrap()
        .to_string()
}

fn neighborhood(root: &str, id: &str) -> Value {
    tsift(&[
        "graph-db",
        "--path",
        root,
        "neighborhood",
        id,
        "--depth",
        "2",
        "--json",
    ])
}

fn node_labels(neighborhood: &Value) -> Vec<(String, String)> {
    report(neighborhood)["nodes"]
        .as_array()
        .unwrap()
        .iter()
        .map(|node| {
            (
                node["kind"].as_str().unwrap_or_default().to_string(),
                node["label"].as_str().unwrap_or_default().to_string(),
            )
        })
        .collect()
}

#[test]
fn haiven_trace_import_makes_contract_blast_radius_one_query() {
    let project = sdk_project();
    let root = project.path().to_string_lossy().to_string();
    tsift(&["graph-db", "--path", &root, "refresh", "--json"]);
    let contract = contract_node_id(&root);

    let imported = tsift(&[
        "graph-db",
        "--path",
        &root,
        "import",
        "codegen/trace.json",
        "--json",
    ]);
    // A relative FILE that is not under the cwd resolves against --path.
    let entries = report(&imported)["imports"].as_array().unwrap();
    assert_eq!(entries.len(), 1, "{imported}");
    let entry = &entries[0];
    assert_eq!(entry["source"], "codegen/trace.json");
    assert_eq!(entry["adapter"], "haiven-trace");
    let projected = &entry["projected"];
    // The contract joins the JSON Schema; the two TypeScript declarations join
    // their indexed symbols by path:line; the event joins nothing indexed.
    assert_eq!(projected["joined_nodes"], "3", "{imported}");
    assert_eq!(projected["external_endpoints"], "0", "{imported}");

    // One query from the indexed contract node: per-language declarations,
    // the frame that carries it, and that frame's conformance scenario.
    let hood = neighborhood(&root, &contract);
    let labels = node_labels(&hood);
    assert!(
        labels.contains(&("symbol".to_string(), "ChatMessageV0".to_string()))
            && report(&hood)["nodes"]
                .as_array()
                .unwrap()
                .iter()
                .any(|node| node["properties"]["path"] == "sdk/contracts.ts"),
        "TypeScript declaration (joined by path:line) missing: {labels:?}"
    );
    assert!(
        labels.contains(&(
            "sdk_declaration".to_string(),
            "ChatMessageV0 (gdscript)".to_string()
        )),
        "unindexed GDScript declaration missing: {labels:?}"
    );
    assert!(
        labels.contains(&("ws_event".to_string(), "chat.message".to_string())),
        "carrying WebSocket event missing: {labels:?}"
    );
    assert!(
        labels.contains(&(
            "conformance_scenario".to_string(),
            "gaming/v0#an-event-carries-no-correlation".to_string()
        )),
        "conformance scenario missing: {labels:?}"
    );

    // A plain refresh keeps the import (it is part of the projection).
    tsift(&[
        "graph-db",
        "--path",
        &root,
        "refresh",
        "--rebuild",
        "--json",
    ]);
    let labels = node_labels(&neighborhood(&root, &contract));
    assert!(
        labels
            .iter()
            .any(|(kind, _)| kind == "conformance_scenario")
    );

    // Re-importing the same source replaces its rows instead of adding to them.
    let count_edges = |root: &str| -> usize {
        let edges = tsift(&[
            "graph-db",
            "--path",
            root,
            "edges",
            "--property",
            "import_source=codegen/trace.json",
            "--limit",
            "0",
            "--json",
        ]);
        // An empty page omits `edges` entirely.
        report(&edges)["edges"].as_array().map_or(0, Vec::len)
    };
    let before = count_edges(&root);
    assert!(before > 0);
    let reimport = Command::new(env!("CARGO_BIN_EXE_tsift-cli"))
        .current_dir(project.path())
        .args(["graph-db", "import", "codegen/trace.json", "--json"])
        .output()
        .unwrap();
    assert!(reimport.status.success());
    assert_eq!(count_edges(&root), before, "re-import must be idempotent");

    // A changed trace replaces the old rows: the scenario edge disappears.
    write_trace(project.path(), false);
    let reimport = Command::new(env!("CARGO_BIN_EXE_tsift-cli"))
        .current_dir(project.path())
        .args(["graph-db", "import", "codegen/trace.json", "--json"])
        .output()
        .unwrap();
    assert!(reimport.status.success());
    let labels = node_labels(&neighborhood(&root, &contract));
    assert!(
        !labels
            .iter()
            .any(|(kind, _)| kind == "conformance_scenario"),
        "stale scenario survived re-import: {labels:?}"
    );

    // --remove drops the source entirely.
    tsift(&[
        "graph-db",
        "--path",
        &root,
        "import",
        "--remove",
        "--source",
        "codegen/trace.json",
        "--json",
    ]);
    assert_eq!(count_edges(&root), 0);
}

#[test]
fn generic_edge_file_counts_unmatched_endpoints_as_external() {
    let project = sdk_project();
    let root = project.path().to_string_lossy().to_string();
    let edges = project.path().join("deps.json");
    fs::write(
        &edges,
        serde_json::to_string_pretty(&json!({
            "format": "tsift-graph-edges/v1",
            "source": "deps",
            "nodes": [
                {"id": "svc", "kind": "service", "label": "chat-service",
                 "properties": {"owner": "chat", "replicas": 3}}
            ],
            "edges": [
                {"from": "svc", "to": "ChatMessageV0", "kind": "consumes"},
                {"from": "svc", "to": "LegacyQueue", "kind": "publishes_to"}
            ]
        }))
        .unwrap(),
    )
    .unwrap();
    let edges_arg = edges.to_string_lossy().to_string();
    let imported = tsift(&["graph-db", "--path", &root, "import", &edges_arg, "--json"]);
    let entry = &report(&imported)["imports"][0];
    assert_eq!(entry["source"], "deps");
    let projected = &entry["projected"];
    // ChatMessageV0 matches two indexed symbols (schema + TypeScript), both linked.
    assert_eq!(projected["matched_endpoints"], "1", "{imported}");
    assert_eq!(projected["ambiguous_joins"], "1", "{imported}");
    assert_eq!(projected["external_endpoints"], "1", "{imported}");
    assert_eq!(projected["edges"], "3", "{imported}");

    let externals = tsift(&["graph-db", "--path", &root, "kind", "external", "--json"]);
    let labels: Vec<&str> = report(&externals)["nodes"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|node| node["label"].as_str())
        .collect();
    assert_eq!(labels, vec!["LegacyQueue"]);

    let services = tsift(&["graph-db", "--path", &root, "kind", "service", "--json"]);
    let service = &report(&services)["nodes"][0];
    assert_eq!(service["properties"]["owner"], "chat");
    assert_eq!(service["properties"]["replicas"], "3");

    let listed = tsift(&["graph-db", "--path", &root, "import", "--list", "--json"]);
    assert_eq!(report(&listed)["imports"].as_array().unwrap().len(), 1);

    fs::write(
        &edges,
        r#"{"edges": [{"from": "a", "to": "b", "kind": ""}]}"#,
    )
    .unwrap();
    let stderr = tsift_fails(&["graph-db", "--path", &root, "import", &edges_arg, "--json"]);
    assert!(stderr.contains("empty `kind`"), "{stderr}");
}

fn git(dir: &Path, args: &[&str]) -> String {
    let output = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args([
            "-c",
            "user.email=t@example.com",
            "-c",
            "user.name=t",
            "-c",
            "commit.gpgsign=false",
        ])
        .args(args)
        .output()
        .unwrap();
    assert!(output.status.success(), "git {args:?} failed: {output:?}");
    String::from_utf8_lossy(&output.stdout).trim().to_string()
}

fn write_lock(root: &Path, commit: &str) {
    fs::write(
        root.join("codegen/contracts.lock"),
        format!("{{\n\t\"repository\": \"haiven-dev/haiven-contracts\",\n\t\"commit\": \"{commit}\"\n}}\n"),
    )
    .unwrap();
}

fn import_warnings(root: &str) -> (Vec<String>, Value) {
    let imported = tsift(&[
        "graph-db",
        "--path",
        root,
        "import",
        "codegen/trace.json",
        "--json",
    ]);
    let warnings = report(&imported)["warnings"]
        .as_array()
        .map(|warnings| {
            warnings
                .iter()
                .filter_map(|warning| warning.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default();
    (warnings, imported)
}

#[test]
fn haiven_trace_contracts_lock_mismatch_warns_and_match_is_silent() {
    let project = sdk_project();
    let root = project.path().to_string_lossy().to_string();
    // haiven-sdk's layout: the contracts submodule beside `codegen/`.
    let contracts = project.path().join("haiven-contracts");
    fs::create_dir_all(&contracts).unwrap();
    git(&contracts, &["init", "-q"]);
    fs::write(contracts.join("README.md"), "contracts\n").unwrap();
    git(&contracts, &["add", "."]);
    git(&contracts, &["commit", "-q", "-m", "first"]);
    let first = git(&contracts, &["rev-parse", "HEAD"]);
    fs::write(contracts.join("README.md"), "contracts v2\n").unwrap();
    git(&contracts, &["commit", "-q", "-am", "second"]);
    let second = git(&contracts, &["rev-parse", "HEAD"]);

    // The lock pins `first` but the checkout is at `second`.
    write_lock(project.path(), &first);
    let (warnings, imported) = import_warnings(&root);
    let expected = format!(
        "codegen/contracts.lock pins haiven-contracts {} but the indexed contracts checkout haiven-contracts is at {}; contract joins may land on declarations the trace was not generated from — check out {} there (or move the lock) and re-import",
        &first[..7],
        &second[..7],
        &first[..7]
    );
    assert!(warnings.contains(&expected), "{imported}");
    // The pin is recorded on the source's summary node.
    assert_eq!(
        report(&imported)["imports"][0]["projected"]["contracts_commit"],
        first.as_str(),
        "{imported}"
    );

    // A lock that matches the checkout imports without a contracts warning.
    write_lock(project.path(), &second);
    let (warnings, imported) = import_warnings(&root);
    assert!(
        !warnings
            .iter()
            .any(|warning| warning.contains("contracts.lock")
                || warning.contains("haiven-contracts")),
        "{imported}"
    );
    assert_eq!(
        report(&imported)["imports"][0]["projected"]["contracts_commit"],
        second.as_str(),
        "{imported}"
    );
}

#[test]
fn haiven_trace_without_contracts_lock_still_imports() {
    let project = sdk_project();
    let root = project.path().to_string_lossy().to_string();
    let (warnings, imported) = import_warnings(&root);
    assert_eq!(report(&imported)["imports"].as_array().unwrap().len(), 1);
    assert!(
        warnings
            .iter()
            .any(|warning| warning.starts_with("no contracts.lock beside codegen/trace.json")),
        "{imported}"
    );
}
