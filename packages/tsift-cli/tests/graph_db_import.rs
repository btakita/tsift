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

/// A project whose agent-doc controller cached decision graphs for two
/// session documents (`#decisiongraphtsift`): `tasks/b.md` depends on a
/// decision only `tasks/a.md` owns, and both mention item `shipx`.
fn agent_doc_project() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    git(root, &["init", "-q"]);
    fs::create_dir_all(root.join("tasks")).unwrap();
    fs::create_dir_all(root.join(".agent-doc")).unwrap();
    fs::write(root.join("tasks/a.md"), "# A\n\n- [#shipx] ship x\n").unwrap();
    fs::write(root.join("tasks/b.md"), "# B\n\nsee [#shipx]\n").unwrap();
    write_agent_doc_state(root, "yes");
    dir
}

/// agent-doc-sqlite `answer_ledger_index` DDL (`#decisiongraph`) plus the
/// controller's `documents` table, in WAL mode like the live controller.
fn write_agent_doc_state(root: &Path, answer: &str) {
    let canonical = fs::canonicalize(root).unwrap();
    let conn = rusqlite::Connection::open(root.join(".agent-doc/state.db")).unwrap();
    conn.execute_batch(
        r#"
        PRAGMA journal_mode = WAL;
        DROP TABLE IF EXISTS answer_ledger_edges;
        DROP TABLE IF EXISTS answer_ledger_nodes;
        DROP TABLE IF EXISTS documents;
        CREATE TABLE answer_ledger_edges (
            document_id TEXT NOT NULL, from_id TEXT NOT NULL, kind TEXT NOT NULL,
            to_id TEXT NOT NULL, recorded_at INTEGER NOT NULL,
            source TEXT NOT NULL DEFAULT 'derived', clause TEXT,
            PRIMARY KEY (document_id, from_id, kind, to_id));
        CREATE TABLE answer_ledger_nodes (
            document_id TEXT NOT NULL, node_id TEXT NOT NULL, kind TEXT NOT NULL,
            label TEXT NOT NULL, open INTEGER, location TEXT, first_seen_at INTEGER,
            recorded_at INTEGER NOT NULL, PRIMARY KEY (document_id, node_id));
        CREATE TABLE documents (document_id TEXT PRIMARY KEY, canonical_path TEXT NOT NULL);
        INSERT INTO answer_ledger_nodes VALUES
            ('da', 'dec-1', 'decision', 'Ship X?', 0, NULL, 5, 1),
            ('da', 'shipx', 'item', 'ship x', 1, NULL, 5, 1),
            ('db', 'dec-2', 'decision', 'When?', 1, NULL, 6, 1),
            ('db', 'shipx', 'item', '', 1, NULL, 6, 1);
        INSERT INTO answer_ledger_edges VALUES
            ('da', 'dec-1', 'applies_to', 'shipx', 1, 'derived', NULL),
            ('db', 'dec-2', 'applies_to', 'shipx', 1, 'derived', NULL),
            ('db', 'dec-2', 'depends_on', 'dec-1', 1, 'operator', NULL);
        "#,
    )
    .unwrap();
    conn.execute(
        "INSERT INTO answer_ledger_nodes VALUES ('da', 'dec-1@r', 'revision', ?1, NULL, 'queue row', 5, 1)",
        [answer],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO answer_ledger_edges VALUES ('da', 'dec-1@r', 'answers', 'dec-1', 1, 'derived', NULL)",
        [],
    )
    .unwrap();
    for (id, name) in [("da", "tasks/a.md"), ("db", "tasks/b.md")] {
        conn.execute(
            "INSERT INTO documents VALUES (?1, ?2)",
            [id, canonical.join(name).to_string_lossy().as_ref()],
        )
        .unwrap();
    }
}

fn decision_id(root: &str, ref_id: &str) -> String {
    let decisions = tsift(&[
        "graph-db",
        "--path",
        root,
        "kind",
        "agent_doc_decision",
        "--property",
        &format!("ref_id={ref_id}"),
        "--json",
    ]);
    report(&decisions)["nodes"][0]["id"]
        .as_str()
        .unwrap_or_else(|| panic!("decision {ref_id} missing: {decisions}"))
        .to_string()
}

#[test]
fn agent_doc_decision_cache_imports_read_only_and_traverses_across_documents() {
    let project = agent_doc_project();
    let root = project.path().to_string_lossy().to_string();
    let state_db = project.path().join(".agent-doc/state.db");
    tsift(&["graph-db", "--path", &root, "refresh", "--json"]);
    let before = fs::read(&state_db).unwrap();

    // FILE defaults to the project's .agent-doc/state.db.
    let imported = tsift(&[
        "graph-db",
        "--path",
        &root,
        "import",
        "--format",
        "agent-doc-decisions",
        "--json",
    ]);
    assert_eq!(
        fs::read(&state_db).unwrap(),
        before,
        "import must not write state.db"
    );
    let entry = &report(&imported)["imports"][0];
    assert_eq!(entry["source"], "agent-doc-decisions", "{imported}");
    assert_eq!(entry["adapter"], "agent-doc-decisions");
    assert_eq!(entry["imported_from"], ".agent-doc/state.db");
    let projected = &entry["projected"];
    assert_eq!(projected["agent_doc_schema"], "agent-doc/decision-graph/v2");
    assert_eq!(projected["agent_doc_documents"], "2");
    // Both session documents join their indexed markdown file nodes.
    assert_eq!(projected["joined_nodes"], "2", "{imported}");
    assert_eq!(projected["external_endpoints"], "0", "{imported}");

    // From b.md's decision: its operator dependency on a.md's decision, the
    // shared item, and both indexed session files.
    let dec2 = decision_id(&root, "dec-2");
    let dec1 = decision_id(&root, "dec-1");
    let hood = neighborhood(&root, &dec2);
    let labels = node_labels(&hood);
    for expected in [
        ("agent_doc_decision", "Ship X?"),
        ("agent_doc_item", "ship x"),
        ("file", "tasks/a.md"),
        ("file", "tasks/b.md"),
    ] {
        assert!(
            labels.contains(&(expected.0.to_string(), expected.1.to_string())),
            "{expected:?} missing from {labels:?}"
        );
    }
    let depends = report(&hood)["edges"]
        .as_array()
        .unwrap()
        .iter()
        .find(|edge| edge["kind"] == "depends_on")
        .unwrap_or_else(|| panic!("depends_on edge missing: {hood}"));
    assert_eq!(depends["from_id"], dec2.as_str());
    assert_eq!(depends["to_id"], dec1.as_str());
    assert_eq!(depends["properties"]["edge_source"], "operator");

    // `auto` recognizes the SQLite file; re-importing is idempotent.
    let count_edges = |root: &str| -> usize {
        let edges = tsift(&[
            "graph-db",
            "--path",
            root,
            "edges",
            "--property",
            "import_source=agent-doc-decisions",
            "--limit",
            "0",
            "--json",
        ]);
        report(&edges)["edges"].as_array().map_or(0, Vec::len)
    };
    let edges_before = count_edges(&root);
    assert!(edges_before > 0);
    let reimport = tsift(&[
        "graph-db",
        "--path",
        &root,
        "import",
        ".agent-doc/state.db",
        "--json",
    ]);
    assert_eq!(
        report(&reimport)["imports"][0]["adapter"],
        "agent-doc-decisions"
    );
    assert_eq!(
        count_edges(&root),
        edges_before,
        "re-import must be idempotent"
    );

    // A changed cache replaces the snapshot: the new answer is what projects.
    write_agent_doc_state(project.path(), "yes, behind a flag");
    tsift(&[
        "graph-db",
        "--path",
        &root,
        "import",
        "--format",
        "agent-doc-decisions",
        "--json",
    ]);
    let revisions = tsift(&[
        "graph-db",
        "--path",
        &root,
        "kind",
        "agent_doc_revision",
        "--json",
    ]);
    let answers: Vec<&str> = report(&revisions)["nodes"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|node| node["label"].as_str())
        .collect();
    assert_eq!(answers, vec!["yes, behind a flag"]);

    // The snapshot is optional: removing it drops every row.
    tsift(&[
        "graph-db",
        "--path",
        &root,
        "import",
        "--remove",
        "--format",
        "agent-doc-decisions",
        "--json",
    ]);
    assert_eq!(count_edges(&root), 0);
}

#[test]
fn agent_doc_import_without_a_state_db_fails_without_creating_one() {
    let dir = tempfile::tempdir().unwrap();
    git(dir.path(), &["init", "-q"]);
    let root = dir.path().to_string_lossy().to_string();
    let stderr = tsift_fails(&[
        "graph-db",
        "--path",
        &root,
        "import",
        "--format",
        "agent-doc-decisions",
        "--json",
    ]);
    assert!(stderr.contains("no agent-doc state database"), "{stderr}");
    assert!(!dir.path().join(".agent-doc").exists());
}

/// `#decisionitembacklogjoin`: the imported item for backlog id `shipx` links
/// to the session document's `backlog` node, so `graph-db evidence shipx`
/// reaches the decisions about it in both documents and the revision that
/// answered one, while a backlog id no decision mentions carries none.
#[test]
fn agent_doc_items_join_backlog_nodes_so_evidence_reaches_their_decisions() {
    let project = agent_doc_project();
    fs::write(
        project.path().join("tasks/a.md"),
        r#"---
agent_doc_session: decision-join
agent_doc_format: template
---

## Backlog

<!-- agent:backlog -->
- [ ] [#shipx] ship x
- [ ] [#lonely] nothing decided about this one
<!-- /agent:backlog -->
"#,
    )
    .unwrap();
    let root = project.path().to_string_lossy().to_string();
    let imported = tsift(&[
        "graph-db",
        "--path",
        &root,
        "import",
        "--format",
        "agent-doc-decisions",
        "--json",
    ]);
    let projected = &report(&imported)["imports"][0]["projected"];
    assert_eq!(projected["linked_nodes"], "1", "{imported}");
    assert_eq!(projected["joined_nodes"], "2", "{imported}");

    // The item stays queryable by kind; the backlog node links to it.
    let items = tsift(&[
        "graph-db",
        "--path",
        &root,
        "kind",
        "agent_doc_item",
        "--property",
        "ref_id=shipx",
        "--json",
    ]);
    let item = report(&items)["nodes"][0]["id"]
        .as_str()
        .unwrap_or_else(|| panic!("item shipx missing: {items}"))
        .to_string();
    let links = tsift(&[
        "graph-db",
        "--path",
        &root,
        "edges",
        "--edge-kind",
        "decision_item",
        "--json",
    ]);
    let link_edges = report(&links)["edges"].as_array().unwrap();
    assert_eq!(link_edges.len(), 1, "{links}");
    assert_eq!(link_edges[0]["to_id"], item.as_str());
    let backlog = link_edges[0]["from_id"].as_str().unwrap().to_string();
    assert!(backlog.starts_with("gbak-"), "{links}");

    let evidence = tsift(&["graph-db", "--path", &root, "evidence", "shipx", "--json"]);
    let packet = report(&evidence);
    assert_eq!(packet["target_node"]["id"], backlog.as_str(), "{evidence}");
    let rows: Vec<(u64, String, String, String)> = packet["decision_graph"]
        .as_array()
        .unwrap_or_else(|| panic!("decision_graph missing: {evidence}"))
        .iter()
        .map(|row| {
            (
                row["hops"].as_u64().unwrap(),
                row["kind"].as_str().unwrap().to_string(),
                row["label"].as_str().unwrap().to_string(),
                row["edge_kind"].as_str().unwrap().to_string(),
            )
        })
        .collect();
    let row = |hops: u64, kind: &str, label: &str, edge: &str| {
        (hops, kind.to_string(), label.to_string(), edge.to_string())
    };
    assert_eq!(
        rows,
        vec![
            row(1, "agent_doc_item", "ship x", "decision_item"),
            row(2, "agent_doc_decision", "Ship X?", "applies_to"),
            row(2, "agent_doc_decision", "When?", "applies_to"),
            row(3, "agent_doc_revision", "yes", "answers"),
        ],
        "{evidence}"
    );
    let dec1 = decision_id(&root, "dec-1");
    let revision = &packet["decision_graph"][3];
    assert_eq!(revision["edge_to"], dec1.as_str());
    assert!(
        packet["next_commands"]
            .as_array()
            .unwrap()
            .iter()
            .any(|command| command
                .as_str()
                .is_some_and(|command| command.contains("incident") && command.contains(&item))),
        "{evidence}"
    );

    // --depth bounds the walk; a backlog id with no decisions has no rows.
    let shallow = tsift(&[
        "graph-db", "--path", &root, "evidence", "shipx", "--depth", "1", "--json",
    ]);
    assert_eq!(
        report(&shallow)["decision_graph"].as_array().map(Vec::len),
        Some(1),
        "{shallow}"
    );
    let lonely = tsift(&["graph-db", "--path", &root, "evidence", "lonely", "--json"]);
    assert!(report(&lonely).get("decision_graph").is_none(), "{lonely}");

    // Removing the import drops the link with the item.
    tsift(&[
        "graph-db",
        "--path",
        &root,
        "import",
        "--remove",
        "--format",
        "agent-doc-decisions",
        "--json",
    ]);
    let links = tsift(&[
        "graph-db",
        "--path",
        &root,
        "edges",
        "--edge-kind",
        "decision_item",
        "--json",
    ]);
    assert_eq!(report(&links)["edges"].as_array().map_or(0, Vec::len), 0);
}

fn stored_import_file(root: &str) -> std::path::PathBuf {
    let listed = tsift(&["graph-db", "--path", root, "import", "--list", "--json"]);
    std::path::PathBuf::from(
        report(&listed)["imports"][0]["stored_file"]
            .as_str()
            .unwrap_or_else(|| panic!("no stored import: {listed}")),
    )
}

fn status_reminders(root: &str, extra: &[&str]) -> Vec<String> {
    let mut args = vec!["status", root, "--json"];
    args.extend_from_slice(extra);
    let status = tsift(&args);
    status["reminders"]
        .as_array()
        .map(|reminders| {
            reminders
                .iter()
                .filter_map(|reminder| reminder.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default()
}

fn revision_answers(root: &str) -> Vec<String> {
    let revisions = tsift(&[
        "graph-db",
        "--path",
        root,
        "kind",
        "agent_doc_revision",
        "--json",
    ]);
    report(&revisions)["nodes"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|node| node["label"].as_str().map(str::to_string))
        .collect()
}

/// `#decisiongraphimportrefresh`: the import is a snapshot, so it used to lag
/// silently as agent-doc cached new decisions (graph freshness stayed
/// `current`, status said nothing). Bare `tsift status` and `graph-db
/// refresh` now catch a stored import up, `--no-fix` reports it, and nothing
/// is rewritten while the decision cache is unchanged, not even when the
/// controller re-stores the same rows.
#[test]
fn agent_doc_decision_import_catches_up_only_when_the_decision_cache_changed() {
    let project = agent_doc_project();
    let root = project.path().to_string_lossy().to_string();
    let state_db = project.path().join(".agent-doc/state.db");
    tsift(&["graph-db", "--path", &root, "refresh", "--json"]);
    tsift(&[
        "graph-db",
        "--path",
        &root,
        "import",
        "--format",
        "agent-doc-decisions",
        "--json",
    ]);
    let stored = stored_import_file(&root);
    let snapshot = fs::read(&stored).unwrap();
    let digest = |bytes: &[u8]| -> String {
        let file: Value = serde_json::from_slice(bytes).unwrap();
        file["properties"]["agent_doc_cache_digest"]
            .as_str()
            .unwrap_or_else(|| panic!("import records no cache digest: {file}"))
            .to_string()
    };
    let first_digest = digest(&snapshot);

    // The controller re-stores the same rows every cycle (only bookkeeping
    // columns move): the import stays current and is not rewritten.
    let conn = rusqlite::Connection::open(&state_db).unwrap();
    conn.execute_batch(
        "UPDATE answer_ledger_nodes SET recorded_at = recorded_at + 100;
         UPDATE answer_ledger_edges SET recorded_at = recorded_at + 100;",
    )
    .unwrap();
    drop(conn);
    let reminders = status_reminders(&root, &[]);
    assert!(
        !reminders.iter().any(|line| line.contains("graph import")),
        "{reminders:?}"
    );
    assert_eq!(
        fs::read(&stored).unwrap(),
        snapshot,
        "unchanged cache rewrote the import"
    );

    // A new answer is cached. `--no-fix` reports the stale import and writes nothing.
    write_agent_doc_state(project.path(), "yes, behind a flag");
    let before_status = fs::read(&state_db).unwrap();
    let reminders = status_reminders(&root, &["--no-fix"]);
    assert!(
        reminders.iter().any(
            |line| line.contains("graph import `agent-doc-decisions` is stale")
                && line.contains("--format agent-doc-decisions")
        ),
        "{reminders:?}"
    );
    assert_eq!(fs::read(&stored).unwrap(), snapshot);
    assert_eq!(revision_answers(&root), vec!["yes"]);

    // Bare status re-lowers the stored import in place (never state.db) and
    // points at the refresh that projects it.
    let reminders = status_reminders(&root, &[]);
    assert!(
        reminders.iter().any(|line| line
            .contains("graph import `agent-doc-decisions` re-imported")
            && line.contains("refresh --json")),
        "{reminders:?}"
    );
    assert_eq!(
        fs::read(&state_db).unwrap(),
        before_status,
        "status wrote state.db"
    );
    let relowered = fs::read(&stored).unwrap();
    assert_ne!(relowered, snapshot);
    assert_ne!(digest(&relowered), first_digest);
    let relowered_file: Value = serde_json::from_slice(&relowered).unwrap();
    assert_eq!(relowered_file["imported_from"], ".agent-doc/state.db");
    assert_eq!(relowered_file["source"], "agent-doc-decisions");
    tsift(&["graph-db", "--path", &root, "refresh", "--json"]);
    assert_eq!(revision_answers(&root), vec!["yes, behind a flag"]);
    // Caught up: the next status is silent and rewrites nothing.
    let reminders = status_reminders(&root, &[]);
    assert!(
        !reminders.iter().any(|line| line.contains("graph import")),
        "{reminders:?}"
    );
    assert_eq!(fs::read(&stored).unwrap(), relowered);

    // `graph-db refresh` alone catches up too, before it takes the watermark.
    write_agent_doc_state(project.path(), "no");
    tsift(&["graph-db", "--path", &root, "refresh", "--json"]);
    assert_eq!(revision_answers(&root), vec!["no"]);
    let caught_up = fs::read(&stored).unwrap();
    let again = tsift(&["graph-db", "--path", &root, "refresh", "--json"]);
    assert_eq!(fs::read(&stored).unwrap(), caught_up);
    assert_eq!(
        report(&again)["refresh"]["mode"],
        "cached_source_watermark_reuse",
        "an unchanged cache must not invalidate the projection: {again}"
    );

    // An unreadable state.db keeps the stored import and never fails status.
    fs::remove_file(&state_db).unwrap();
    for sidecar in ["state.db-wal", "state.db-shm"] {
        let _ = fs::remove_file(project.path().join(".agent-doc").join(sidecar));
    }
    let reminders = status_reminders(&root, &[]);
    assert!(
        reminders
            .iter()
            .any(|line| line.contains("kept as stored")
                && line.contains("no agent-doc state database")),
        "{reminders:?}"
    );
    assert_eq!(fs::read(&stored).unwrap(), caught_up);
    assert!(!state_db.exists(), "the check must not create state.db");
}
