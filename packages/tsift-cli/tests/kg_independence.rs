//! Hermetic guard: core code-RAG commands must work without `tsift kg`.
//!
//! Locks the "tsift works without `tsift kg`" invariant by name (backlog
//! `#5hed`). The everyday code-RAG workflow (status / search / explain /
//! graph / source-read / session-review) must:
//!   1. succeed (exit 0) in a workspace that has NO `.tsift/graph.db`, and
//!   2. never reach a local KG model — Ollama is pointed at a closed port so
//!      any accidental model contact would hang/fail the command, and
//!   3. never create a `.tsift/graph.db` as a side effect.
//!
//! The full hermetic suite already runs with no graph.db and no Ollama, so the
//! invariant is *implicitly* covered everywhere; this test asserts it
//! *explicitly* so a regression that wires the KG substrate into a core command
//! fails here with a named diagnostic instead of silently.

use std::fs;
use std::path::Path;
use std::process::{Command, Output};

fn tsift_bin() -> Command {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_tsift-cli"));
    // Point the local KG model host at a closed port so any code path that
    // tried to reach Ollama would fail fast (and fail this test) rather than
    // succeed by accident against a developer's running daemon.
    cmd.env("OLLAMA_HOST", "http://127.0.0.1:1");
    cmd
}

fn build_fixture(dir: &Path) {
    fs::write(
        dir.join("main.rs"),
        r#"fn main() {
    greet();
}

fn greet() {
    helper();
}

fn helper() {}
"#,
    )
    .unwrap();
}

fn assert_ok(label: &str, output: &Output) {
    assert!(
        output.status.success(),
        "core command `{label}` must succeed without tsift kg (exit {:?})\nstdout: {}\nstderr: {}",
        output.status.code(),
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
}

#[test]
fn core_commands_work_without_tsift_kg_and_create_no_graph_db() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    build_fixture(root);

    // A scratch HOME so session-review never scans the developer's real
    // Codex/Claude session history.
    let home = tempfile::tempdir().unwrap();

    let graph_db = root.join(".tsift/graph.db");

    // Build the core code-RAG index. This populates `.tsift/index`, never
    // `.tsift/graph.db` (a separate, optional KG projection).
    let out = tsift_bin()
        .args(["index", root.to_str().unwrap()])
        .output()
        .unwrap();
    assert_ok("index", &out);
    assert!(
        !graph_db.exists(),
        "indexing must not create .tsift/graph.db"
    );

    // status
    let out = tsift_bin()
        .args(["status", root.to_str().unwrap()])
        .output()
        .unwrap();
    assert_ok("status", &out);

    // search
    let out = tsift_bin()
        .args(["search", "--path", root.to_str().unwrap(), "greet"])
        .output()
        .unwrap();
    assert_ok("search", &out);

    // explain
    let out = tsift_bin()
        .args(["explain", "greet", root.to_str().unwrap()])
        .output()
        .unwrap();
    assert_ok("explain", &out);

    // graph
    let out = tsift_bin()
        .args(["graph", "greet", root.to_str().unwrap(), "--callees"])
        .output()
        .unwrap();
    assert_ok("graph", &out);

    // source-read
    let out = tsift_bin()
        .args(["source-read", "main.rs", "--path", root.to_str().unwrap()])
        .output()
        .unwrap();
    assert_ok("source-read", &out);

    // session-review over a plain notes doc; `graph_evidence` is gated on a
    // graph.db being present, so it is simply omitted here — the command must
    // still succeed.
    let notes = root.join("notes.md");
    fs::write(&notes, "# notes\n\nsummarize the tsift session\n").unwrap();
    let out = tsift_bin()
        .args(["session-review", "--json", notes.to_str().unwrap()])
        .env("HOME", home.path())
        .output()
        .unwrap();
    assert_ok("session-review", &out);

    // The core path must never have created the KG substrate.
    assert!(
        !graph_db.exists(),
        "no core command may create .tsift/graph.db; found one at {}",
        graph_db.display()
    );
}
