//! Integration tests for `tsift finding` — Phase 1 of the Findings Graph
//! Layer (#trt1p1). The durability invariant (findings survive a code-graph
//! projection refresh) is the centerpiece and is tested first.

use std::fs;
use std::path::Path;
use std::process::Command;

fn tsift_bin() -> Command {
    Command::new(env!("CARGO_BIN_EXE_tsift-cli"))
}

fn run(args: &[&str], cwd: &Path) -> std::process::Output {
    tsift_bin().args(args).current_dir(cwd).output().unwrap()
}

fn run_ok(args: &[&str], cwd: &Path) -> String {
    let output = run(args, cwd);
    assert!(
        output.status.success(),
        "command {args:?} failed: stdout={} stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
    String::from_utf8_lossy(&output.stdout).to_string()
}

fn write_alpha_beta(root: &Path, alpha_body: &str) {
    fs::write(
        root.join("main.rs"),
        format!(
            "pub fn alpha() -> i32 {{\n    {alpha_body}\n}}\n\npub fn beta() -> i32 {{\n    alpha()\n}}\n"
        ),
    )
    .unwrap();
}

fn indexed_fixture(alpha_body: &str) -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    write_alpha_beta(dir.path(), alpha_body);
    let output = run(&["index", "."], dir.path());
    assert!(
        output.status.success(),
        "index failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    dir
}

fn add_finding(root: &Path, kind: &str, title: &str, about: &str) -> serde_json::Value {
    let out = run_ok(
        &[
            "finding", "add", "--path", ".", "--kind", kind, "--title", title, "--body",
            "the why", "--about", about, "--json",
        ],
        root,
    );
    serde_json::from_str(&out).unwrap()
}

fn list_json(root: &Path, extra: &[&str]) -> serde_json::Value {
    let mut args = vec!["finding", "list", ".", "--json"];
    args.extend_from_slice(extra);
    let out = run_ok(&args, root);
    serde_json::from_str(&out).unwrap()
}

/// Initialize a committed git repo so `context-pack`'s diff digest can run.
fn init_git(root: &Path) {
    let run_git = |args: &[&str]| {
        let status = Command::new("git").args(args).current_dir(root).status().unwrap();
        assert!(status.success(), "git {args:?} failed");
    };
    run_git(&["init"]);
    run_git(&["add", "."]);
    run_git(&[
        "-c", "user.name=tsift-tests", "-c", "user.email=tests@tsift", "commit", "-m", "init",
    ]);
}

/// Write a session document whose exchange names `main.rs` as a touched file so
/// the context-pack result set includes the `main.rs` node (the anchor a
/// file-scoped finding rides in on for hot-path injection, #trt1p2).
fn write_session_doc(root: &Path) {
    let task_dir = root.join("tasks/software");
    fs::create_dir_all(&task_dir).unwrap();
    fs::write(
        task_dir.join("tsift.md"),
        r#"---
agent_doc_session: tsift-v0.1
agent_doc_format: template
---

## Exchange

<!-- agent:exchange patch=append -->
❯ do [#kgnv]
Completed `#kgnv`; touched files `main.rs`; tests `cargo test`; follow-up `#gfix`.
<!-- /agent:exchange -->

## Backlog

<!-- agent:backlog -->
- [ ] [#kgnv] Keep the alpha/beta wiring intentional.
<!-- /agent:backlog -->
"#,
    )
    .unwrap();
}

fn context_pack_json(root: &Path) -> serde_json::Value {
    let out = run_ok(
        &[
            "context-pack",
            "tasks/software/tsift.md",
            "--json",
            "--budget",
            "normal",
        ],
        root,
    );
    serde_json::from_str(&out).unwrap()
}

fn finding_titles(pack: &serde_json::Value) -> Vec<String> {
    pack["findings"]
        .as_array()
        .map(|items| {
            items
                .iter()
                .map(|item| item["title"].as_str().unwrap_or_default().to_string())
                .collect()
        })
        .unwrap_or_default()
}

/// Phase 2 (#trt1p2) hot-path injection: a trusted, fresh finding anchored to a
/// node already in the context-pack result set rides along in the same envelope.
#[test]
fn context_pack_injects_trusted_fresh_finding_for_result_set_node() {
    let dir = indexed_fixture("1");
    let root = dir.path();
    init_git(root);
    write_session_doc(root);

    add_finding(root, "decision", "main.rs owns the entrypoint wiring", "main.rs");

    let pack = context_pack_json(root);
    let titles = finding_titles(&pack);
    assert!(
        titles.contains(&"main.rs owns the entrypoint wiring".to_string()),
        "expected the trusted fresh finding injected, got findings: {:?}",
        pack["findings"]
    );
    // The injected preview carries the anchor + an expand command back to the
    // full finding set.
    let injected = pack["findings"]
        .as_array()
        .unwrap()
        .iter()
        .find(|item| item["about"].as_str() == Some("main.rs"))
        .unwrap();
    assert_eq!(injected["anchor_kind"].as_str().unwrap(), "file");
    assert!(
        injected["expand"]
            .as_str()
            .unwrap()
            .contains("tsift finding list --about"),
        "expand should resolve the full finding set: {injected}"
    );
}

/// Draft findings (including future passive-harvest drafts) must never ride the
/// hot path — only `trusted` authored context is injected.
#[test]
fn context_pack_excludes_draft_findings() {
    let dir = indexed_fixture("1");
    let root = dir.path();
    init_git(root);
    write_session_doc(root);

    run_ok(
        &[
            "finding", "add", "--path", ".", "--kind", "note", "--title",
            "draft about main", "--body", "unverified", "--about", "main.rs", "--status",
            "draft",
        ],
        root,
    );

    let pack = context_pack_json(root);
    assert!(
        !finding_titles(&pack).contains(&"draft about main".to_string()),
        "draft finding must not be injected: {:?}",
        pack["findings"]
    );
}

/// A finding whose anchor has advanced past its captured watermark is stale and
/// must not be injected — stale-but-hidden over wrong-and-confident on the hot
/// path.
#[test]
fn context_pack_excludes_stale_findings() {
    let dir = indexed_fixture("1");
    let root = dir.path();
    init_git(root);
    write_session_doc(root);

    add_finding(root, "decision", "main.rs stale candidate", "main.rs");

    // Mutate main.rs so the file watermark advances; the finding goes stale.
    write_alpha_beta(root, "99");
    run_ok(&["index", "."], root);

    let pack = context_pack_json(root);
    assert!(
        !finding_titles(&pack).contains(&"main.rs stale candidate".to_string()),
        "stale finding must not be injected: {:?}",
        pack["findings"]
    );
}

/// A trusted, fresh finding about a node that is NOT in the result set must not
/// be injected — injection rides the existing result set, it does not dump the
/// whole store.
#[test]
fn context_pack_excludes_findings_outside_result_set() {
    let dir = indexed_fixture("1");
    let root = dir.path();
    init_git(root);
    write_session_doc(root);

    // `beta` is a real symbol but the session exchange only names main.rs, so
    // beta is not in the result set.
    add_finding(root, "note", "beta is unrelated here", "beta");

    let pack = context_pack_json(root);
    assert!(
        !finding_titles(&pack).contains(&"beta is unrelated here".to_string()),
        "finding outside the result set must not be injected: {:?}",
        pack["findings"]
    );
}

/// THE durability invariant: an authored finding lives in `.tsift/findings.db`
/// and MUST survive a code-graph projection refresh of `.tsift/graph.db`
/// (which tombstones non-projected rows). This is the whole reason findings
/// are stored in a dedicated DB instead of the code projection.
#[test]
fn finding_survives_code_graph_projection_refresh() {
    let dir = indexed_fixture("1");
    let root = dir.path();

    let added = add_finding(root, "decision", "alpha returns one", "alpha");
    let id = added["id"].as_str().unwrap().to_string();
    assert!(id.starts_with("finding:"));
    assert!(root.join(".tsift/findings.db").exists());

    // Refresh / materialize the code projection at .tsift/graph.db. This
    // rebuilds the projection and tombstones any non-projected rows in
    // graph.db — findings live in a different DB and must be untouched.
    run_ok(&["graph-db", "--path", ".", "--json", "refresh"], root);
    assert!(
        root.join(".tsift/graph.db").exists(),
        "graph-db refresh should materialize the code projection"
    );

    // Re-index the code (changed-file reindex) to exercise the projection
    // rebuild path again.
    run_ok(&["index", "."], root);
    run_ok(&["graph-db", "--path", ".", "--json", "refresh"], root);

    let listed = list_json(root, &["--include-stale"]);
    assert_eq!(listed["total"].as_u64().unwrap(), 1, "finding must survive");
    assert_eq!(listed["findings"][0]["id"].as_str().unwrap(), id);
    assert_eq!(
        listed["findings"][0]["title"].as_str().unwrap(),
        "alpha returns one"
    );
}

#[test]
fn finding_add_list_roundtrip() {
    let dir = indexed_fixture("1");
    let root = dir.path();

    let added = add_finding(root, "finding", "alpha note", "alpha");
    assert_eq!(added["kind"].as_str().unwrap(), "finding");
    assert_eq!(added["anchor_kind"].as_str().unwrap(), "code_symbol");
    assert_eq!(added["anchor_node"].as_str().unwrap(), "code_symbol:alpha");
    assert!(added["watermark"].as_str().is_some());

    let listed = list_json(root, &[]);
    assert_eq!(listed["total"].as_u64().unwrap(), 1);
    let item = &listed["findings"][0];
    assert_eq!(item["body"].as_str().unwrap(), "the why");
    assert_eq!(item["about"].as_str().unwrap(), "alpha");
    assert!(!item["stale"].as_bool().unwrap());
    assert_eq!(item["status"].as_str().unwrap(), "trusted");
}

#[test]
fn finding_supports_three_node_kinds() {
    let dir = indexed_fixture("1");
    let root = dir.path();

    add_finding(root, "finding", "f-title", "alpha");
    add_finding(root, "decision", "d-title", "alpha");
    add_finding(root, "note", "n-title", "beta");

    let listed = list_json(root, &[]);
    assert_eq!(listed["total"].as_u64().unwrap(), 3);

    let kinds: Vec<&str> = listed["findings"]
        .as_array()
        .unwrap()
        .iter()
        .map(|item| item["kind"].as_str().unwrap())
        .collect();
    assert!(kinds.contains(&"finding"));
    assert!(kinds.contains(&"decision"));
    assert!(kinds.contains(&"note"));

    // Filter by kind.
    let only_notes = list_json(root, &["--kind", "note"]);
    assert_eq!(only_notes["total"].as_u64().unwrap(), 1);
    assert_eq!(
        only_notes["findings"][0]["kind"].as_str().unwrap(),
        "note"
    );
}

#[test]
fn finding_relates_to_edge_threads_findings() {
    let dir = indexed_fixture("1");
    let root = dir.path();

    let first = add_finding(root, "decision", "base decision", "alpha");
    let first_id = first["id"].as_str().unwrap().to_string();

    let out = run_ok(
        &[
            "finding", "add", "--path", ".", "--kind", "note", "--title", "follow up", "--body",
            "depends on base", "--about", "beta", "--relates", &first_id, "--json",
        ],
        root,
    );
    let second: serde_json::Value = serde_json::from_str(&out).unwrap();
    assert_eq!(second["relates_to"].as_str().unwrap(), first_id);

    let listed = list_json(root, &["--about", "beta"]);
    let related = &listed["findings"][0]["relates_to"];
    assert_eq!(related.as_array().unwrap().len(), 1);
    assert_eq!(related[0].as_str().unwrap(), first_id);
}

#[test]
fn finding_relates_to_unknown_target_fails() {
    let dir = indexed_fixture("1");
    let root = dir.path();

    let output = run(
        &[
            "finding", "add", "--path", ".", "--kind", "note", "--title", "x", "--body", "y",
            "--about", "alpha", "--relates", "finding:does-not-exist",
        ],
        root,
    );
    assert!(!output.status.success());
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("not found"),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn finding_flags_stale_when_anchor_changes() {
    let dir = indexed_fixture("1");
    let root = dir.path();

    add_finding(root, "decision", "alpha invariant", "alpha");

    // Default listing shows it as fresh.
    let before = list_json(root, &[]);
    assert_eq!(before["total"].as_u64().unwrap(), 1);
    assert!(!before["findings"][0]["stale"].as_bool().unwrap());

    // Move the anchor's body and reindex; the watermark advances.
    write_alpha_beta(root, "42");
    run_ok(&["index", "."], root);

    // By default stale findings are hidden.
    let hidden = list_json(root, &[]);
    assert_eq!(
        hidden["total"].as_u64().unwrap(),
        0,
        "stale findings hidden by default"
    );

    // With --include-stale they appear, flagged stale.
    let shown = list_json(root, &["--include-stale"]);
    assert_eq!(shown["total"].as_u64().unwrap(), 1);
    let item = &shown["findings"][0];
    assert!(item["stale"].as_bool().unwrap());
    assert_ne!(
        item["captured_watermark"].as_str().unwrap(),
        item["current_watermark"].as_str().unwrap()
    );
}

#[test]
fn finding_list_missing_db_is_empty_not_error() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    // No index, no findings.db.
    let out = run_ok(&["finding", "list", ".", "--json"], root);
    let listed: serde_json::Value = serde_json::from_str(&out).unwrap();
    assert_eq!(listed["total"].as_u64().unwrap(), 0);
    assert!(listed["findings"].as_array().unwrap().is_empty());
}

#[test]
fn finding_add_rejects_invalid_kind_status_confidence() {
    let dir = indexed_fixture("1");
    let root = dir.path();

    let bad_kind = run(
        &[
            "finding", "add", "--path", ".", "--kind", "bogus", "--title", "t", "--body", "b",
            "--about", "alpha",
        ],
        root,
    );
    assert!(!bad_kind.status.success());

    let bad_conf = run(
        &[
            "finding", "add", "--path", ".", "--kind", "note", "--title", "t", "--body", "b",
            "--about", "alpha", "--confidence", "2.0",
        ],
        root,
    );
    assert!(!bad_conf.status.success());
}

#[test]
fn finding_anchors_to_file_when_symbol_unknown() {
    let dir = indexed_fixture("1");
    let root = dir.path();

    let added = add_finding(root, "note", "module note", "main.rs");
    assert_eq!(added["anchor_kind"].as_str().unwrap(), "file");
    assert_eq!(added["anchor_node"].as_str().unwrap(), "file:main.rs");
    // main.rs exists, so a watermark is captured.
    assert!(added["watermark"].as_str().is_some());
}
