use fs4::fs_std::FileExt;
use rusqlite::Connection;
use std::fs;
use std::fs::OpenOptions;
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::process::Command;
use std::time::{Duration, Instant};

fn tsift_bin() -> Command {
    Command::new(env!("CARGO_BIN_EXE_tsift-cli"))
}

fn run_tsift_stdin(args: &[&str], input: &str) -> std::process::Output {
    let mut child = tsift_bin()
        .args(args)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    {
        use std::io::Write;
        child
            .stdin
            .as_mut()
            .unwrap()
            .write_all(input.as_bytes())
            .unwrap();
    }
    child.wait_with_output().unwrap()
}

fn structured_rows(value: &serde_json::Value) -> Vec<serde_json::Value> {
    if let Some(rows) = value.as_array() {
        return rows.clone();
    }

    let columns = value["_c"]
        .as_array()
        .unwrap_or_else(|| panic!("expected structured table columns or row array: {value}"));
    let rows = value["_r"]
        .as_array()
        .unwrap_or_else(|| panic!("expected structured table rows or row array: {value}"));

    rows.iter()
        .map(|row| {
            let values = row
                .as_array()
                .unwrap_or_else(|| panic!("expected structured table row values: {row}"));
            let object = columns
                .iter()
                .zip(values)
                .map(|(column, value)| {
                    (
                        column
                            .as_str()
                            .unwrap_or_else(|| panic!("expected string column: {column}"))
                            .to_string(),
                        value.clone(),
                    )
                })
                .collect();
            serde_json::Value::Object(object)
        })
        .collect()
}

fn hold_rollback_journal_lock(db_path: &std::path::Path) -> Connection {
    let conn = Connection::open(db_path).unwrap();
    conn.execute_batch("PRAGMA journal_mode=DELETE; BEGIN EXCLUSIVE;")
        .unwrap();
    fs::write(format!("{}-journal", db_path.display()), "locked").unwrap();
    conn
}

fn hold_wal_lock(db_path: &std::path::Path) -> Connection {
    let conn = Connection::open(db_path).unwrap();
    conn.execute_batch(
        "PRAGMA journal_mode=WAL;
         PRAGMA wal_autocheckpoint=0;
         CREATE TABLE IF NOT EXISTS wal_lock_probe (id INTEGER PRIMARY KEY);
         INSERT INTO wal_lock_probe DEFAULT VALUES;
         PRAGMA locking_mode=EXCLUSIVE;
         BEGIN EXCLUSIVE;",
    )
    .unwrap();
    conn
}

fn hold_writer_lock(lock_path: &std::path::Path) -> std::fs::File {
    let mut file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(lock_path)
        .unwrap();
    assert!(file.try_lock_exclusive().unwrap());
    use std::io::Write;
    writeln!(file, "{}", std::process::id()).unwrap();
    file
}

fn process_exists(pid: u32) -> bool {
    #[cfg(target_os = "linux")]
    {
        std::path::Path::new(&format!("/proc/{pid}")).exists()
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = pid;
        false
    }
}

fn wait_for_process_exit(pid: u32, timeout: Duration) -> bool {
    let started = Instant::now();
    while started.elapsed() < timeout {
        if !process_exists(pid) {
            return true;
        }
        std::thread::sleep(Duration::from_millis(25));
    }
    !process_exists(pid)
}

#[test]
fn release_publish_gate_requires_secret_variable_and_dry_run() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let workflow = fs::read_to_string(root.join(".github/workflows/release.yml")).unwrap();
    // Release Workflow detail moved to the release sibling spec during the SPEC split.
    let spec = fs::read_to_string(root.join("../../specs/release-integration.md")).unwrap();
    let readme = fs::read_to_string(root.join("README.md")).unwrap();

    assert!(
        workflow.contains("cargo package -p \"$package\" --locked --allow-dirty --list"),
        "release verification should list each split crate package payload"
    );
    assert!(
        workflow.contains("cargo publish -p \"$package\" --locked --dry-run")
            && workflow.contains("output=\"$(cargo publish -p \"$package\" --locked 2>&1)\""),
        "publish job should dry-run each split crate immediately before upload"
    );
    assert!(
        workflow.contains("cargo info --registry crates-io \"$package@$release_version\"")
            && workflow.contains("already exists on crates.io; skipping"),
        "publish job should be resumable when earlier split crates are already published"
    );
    assert!(
        workflow.contains("Too Many Requests|rate limit|try again after")
            && workflow.contains("retrying in 180 seconds"),
        "publish job should retry crates.io rate limits"
    );
    assert!(
        workflow.contains("cargo build -p tsift --release --locked"),
        "release asset builds should target the public root package"
    );
    assert!(
        workflow.contains("vars.TSIFT_ENABLE_CRATES_PUBLISH == 'true'"),
        "publish job should remain opt-in through the repo variable"
    );
    // The publish job authenticates via OIDC trusted publishing (no long-lived
    // CARGO_REGISTRY_TOKEN secret to expire — root-fix for #1td4); it requests an
    // id-token, exchanges it through the crates.io auth action, and feeds the
    // resulting short-lived token to cargo.
    assert!(
        workflow.contains("id-token: write")
            && workflow.contains("rust-lang/crates-io-auth-action@v1")
            && workflow.contains("CARGO_REGISTRY_TOKEN: ${{ steps.auth.outputs.token }}"),
        "publish job should authenticate to crates.io via OIDC trusted publishing"
    );
    assert!(
        !workflow.contains("secrets.CARGO_REGISTRY_TOKEN"),
        "publish job must not depend on a long-lived CARGO_REGISTRY_TOKEN secret"
    );
    assert!(
        spec.contains("TSIFT_ENABLE_CRATES_PUBLISH=true")
            && spec.contains("OIDC trusted publishing")
            && spec.contains("cargo package -p <crate> --locked --allow-dirty --list")
            && spec.contains("cargo publish -p <crate> --locked --dry-run")
            && spec.contains("skips crate versions that already exist on crates.io")
            && spec.contains("retries crates.io rate limits"),
        "release spec should document the OIDC publish gate"
    );
    assert!(
        readme.contains("TSIFT_ENABLE_CRATES_PUBLISH=true")
            && readme.contains("OIDC trusted publishing")
            && readme.contains("skips crate versions that already exist on crates.io")
            && readme.contains("retries crates.io rate limits"),
        "README should document the repo variable and OIDC publishing"
    );

    assert_release_package_order(&workflow, "      - name: Crate package file check");
    assert_release_package_order(&workflow, "      - name: Publish crate");
}

#[test]
fn spec_documents_lazily_rs_cache_contracts() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let packaged_spec = fs::read_to_string(root.join("SPEC.md")).unwrap();
    let workspace_spec = fs::read_to_string(root.join("../../SPEC.md")).unwrap();
    assert_eq!(
        packaged_spec, workspace_spec,
        "packaged CLI spec index should stay in sync with the workspace spec index"
    );

    // lazily-rs cache contracts moved to the graph sibling spec during the SPEC split.
    let spec = fs::read_to_string(root.join("../../specs/graph.md")).unwrap();
    let required = [
        "### lazily-rs Cache Contracts",
        "`SummaryCache` (`packages/tsift-summarize`)",
        "Normalized project-relative file key",
        "summary_cache_reuses_file_snapshot_until_content_hash_changes",
        "`InspectScopeGuard` / `IndexDb::inspect_read_only` (`packages/tsift-index`)",
        "`(db_path, root, prune)` plus a thread-local scope epoch Cell",
        "inspect_scope_lazily_reuses_until_epoch_invalidation",
        "`StatusCheckCache` (`packages/tsift-status`)",
        "StatusInspectKey { db_path, root, prune }",
        "status_cache_reuses_index_inspection_until_invalidated",
        "`ResolveEdgesCache` (`packages/tsift-graph`)",
        "`(file, content_hash)` plus a per-slot mtime Cell",
        "resolve_edges_cache_reuses_slots_until_mtime_or_hash_changes",
        "`GraphStore::neighborhood` (`packages/tsift-core`)",
        "graph_store_contract_covers_crud_neighborhood_and_ordering",
        "`GraphStore::ranked_neighborhood` (`packages/tsift-core`)",
        "ranked_neighborhood_breadth_first_respects_max_nodes",
    ];
    for needle in required {
        assert!(
            spec.contains(needle),
            "lazily-rs cache contract table missing {needle:?}"
        );
    }
}

fn release_crate_order() -> &'static [&'static str] {
    &[
        "tsift-core",
        "tsift-md-ast",
        "tsift-graph",
        "tsift-sqlite",
        "tsift-algorithms",
        "tsift-resolution",
        "tsift-cache",
        "tsift-tokensave",
        "tsift-libsql",
        "tsift-index",
        "tsift-summarize",
        "tsift-quality",
        "tsift-agent-doc",
        "tsift-digest",
        "tsift-search",
        "tsift-status",
        "tsift-session",
        "tsift-memory",
        "tsift-memgraphrag",
        "tsift-surrealdb",
        "tsift-cli",
        "tsift",
        "tsift-sim-world",
    ]
}

fn assert_release_package_order(workflow: &str, step_name: &str) {
    let step = workflow
        .split(step_name)
        .nth(1)
        .unwrap_or_else(|| panic!("release workflow should include {step_name}"));
    let mut previous = 0;
    for package in release_crate_order() {
        let line_with_continuation = format!("            {package} \\");
        let line_last = format!("            {package}\n");
        let next = step[previous..]
            .find(&line_with_continuation)
            .or_else(|| step[previous..].find(&line_last));
        let Some(index) = next.map(|idx| previous + idx) else {
            panic!("release workflow missing dependency-ordered package {package} in {step_name}");
        };
        previous = index;
    }
}

#[test]
fn split_crate_manifests_are_publish_ready() {
    // Workspace crates version in lockstep with tsift-cli; derive from the build
    // env so a release bump doesn't require editing this test.
    let expected_version = env!("CARGO_PKG_VERSION");
    let manifest_dir = Path::new(env!("CARGO_MANIFEST_DIR"));
    let workspace_root = manifest_dir
        .parent()
        .and_then(Path::parent)
        .expect("tsift-cli lives under packages/");

    for (name, rel_manifest) in [
        ("tsift", "Cargo.toml"),
        ("tsift-agent-doc", "packages/tsift-agent-doc/Cargo.toml"),
        ("tsift-algorithms", "packages/tsift-algorithms/Cargo.toml"),
        ("tsift-cache", "packages/tsift-cache/Cargo.toml"),
        ("tsift-cli", "packages/tsift-cli/Cargo.toml"),
        ("tsift-core", "packages/tsift-core/Cargo.toml"),
        ("tsift-digest", "packages/tsift-digest/Cargo.toml"),
        ("tsift-graph", "packages/tsift-graph/Cargo.toml"),
        ("tsift-index", "packages/tsift-index/Cargo.toml"),
        ("tsift-libsql", "packages/tsift-libsql/Cargo.toml"),
        ("tsift-md-ast", "packages/tsift-md-ast/Cargo.toml"),
        ("tsift-memory", "packages/tsift-memory/Cargo.toml"),
        ("tsift-memgraphrag", "packages/tsift-memgraphrag/Cargo.toml"),
        ("tsift-quality", "packages/tsift-quality/Cargo.toml"),
        ("tsift-resolution", "packages/tsift-resolution/Cargo.toml"),
        ("tsift-search", "packages/tsift-search/Cargo.toml"),
        ("tsift-session", "packages/tsift-session/Cargo.toml"),
        ("tsift-sim-world", "packages/tsift-sim-world/Cargo.toml"),
        ("tsift-sqlite", "packages/tsift-sqlite/Cargo.toml"),
        ("tsift-status", "packages/tsift-status/Cargo.toml"),
        ("tsift-summarize", "packages/tsift-summarize/Cargo.toml"),
        ("tsift-surrealdb", "packages/tsift-surrealdb/Cargo.toml"),
        ("tsift-tokensave", "packages/tsift-tokensave/Cargo.toml"),
    ] {
        let manifest_path = workspace_root.join(rel_manifest);
        let manifest = fs::read_to_string(&manifest_path).unwrap();
        let value: toml::Value = manifest.parse().unwrap();
        let package = value
            .get("package")
            .and_then(toml::Value::as_table)
            .unwrap();

        assert_eq!(
            package.get("name").and_then(toml::Value::as_str),
            Some(name),
            "manifest name mismatch for {rel_manifest}"
        );
        assert_eq!(
            package.get("version").and_then(toml::Value::as_str),
            Some(expected_version),
            "manifest version drift for {name}"
        );
        assert_eq!(
            package.get("publish").and_then(toml::Value::as_bool),
            Some(true),
            "publish should be explicit for {name}"
        );
        let readme = package
            .get("readme")
            .and_then(toml::Value::as_str)
            .expect("readme metadata required");
        assert!(
            manifest_path.parent().unwrap().join(readme).exists(),
            "readme path should exist for {name}: {readme}"
        );
        assert!(
            package
                .get("keywords")
                .and_then(toml::Value::as_array)
                .is_some_and(|items| !items.is_empty()),
            "keywords should be explicit for {name}"
        );
        assert!(
            package
                .get("categories")
                .and_then(toml::Value::as_array)
                .is_some_and(|items| !items.is_empty()),
            "categories should be explicit for {name}"
        );

        for table_name in ["dependencies", "dev-dependencies"] {
            if let Some(table) = value.get(table_name).and_then(toml::Value::as_table) {
                for (dep_name, dep_value) in table {
                    let is_local_tsift = dep_name == "tsift" || dep_name.starts_with("tsift-");
                    if is_local_tsift
                        && dep_value
                            .get("path")
                            .and_then(toml::Value::as_str)
                            .is_some()
                    {
                        assert_eq!(
                            dep_value.get("version").and_then(toml::Value::as_str),
                            Some(expected_version),
                            "{name} {table_name}.{dep_name} path dependency needs matching version"
                        );
                    }
                }
            }
        }
    }
}

#[test]
fn cli_manifest_uses_split_crates_without_root_shim() {
    let manifest_path = Path::new(env!("CARGO_MANIFEST_DIR")).join("Cargo.toml");
    let manifest = fs::read_to_string(manifest_path).unwrap();
    let value: toml::Value = manifest.parse().unwrap();
    let deps = value
        .get("dependencies")
        .and_then(toml::Value::as_table)
        .unwrap();

    assert!(
        !deps.contains_key("tsift"),
        "tsift-cli should import sibling crates directly instead of the root re-export shim"
    );
    for name in [
        "tsift-agent-doc",
        "tsift-algorithms",
        "tsift-cache",
        "tsift-core",
        "tsift-digest",
        "tsift-graph",
        "tsift-index",
        "tsift-memgraphrag",
        "tsift-quality",
        "tsift-resolution",
        "tsift-search",
        "tsift-sqlite",
        "tsift-status",
        "tsift-summarize",
        "tsift-tokensave",
    ] {
        assert!(
            deps.contains_key(name),
            "missing direct dependency on {name}"
        );
    }
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

fn init_rust_library_crate(path: &Path) {
    fs::create_dir_all(path.join("src")).unwrap();
    fs::write(
        path.join("Cargo.toml"),
        r#"[package]
name = "tsift-runner-fixture"
version = "0.1.0"
edition = "2021"

[lib]
path = "src/lib.rs"
"#,
    )
    .unwrap();
    fs::write(
        path.join("src/lib.rs"),
        r#"pub fn add(left: i32, right: i32) -> i32 {
    left + right
}

#[cfg(test)]
mod tests {
    use super::add;

    #[test]
    fn adds_numbers() {
        assert_eq!(add(1, 2), 3);
    }
}
"#,
    )
    .unwrap();
}

fn build_cli_fixture(dir: &Path) {
    fs::write(
        dir.join("main.rs"),
        r#"fn main() {
    alpha();
    bridge();
}

fn alpha() {
    beta();
    gamma();
}

fn beta() {
    alpha();
    gamma();
}

fn gamma() {
    alpha();
    beta();
}

fn bridge() {
    shared();
}

fn shared() {
    helper();
}

fn helper() {}

fn delta() {
    epsilon();
}

fn epsilon() {
    delta();
}
"#,
    )
    .unwrap();
}

/// Build and persist a fresh tagpath index at `root` so the tsift tagpath
/// adapter loads cleanly. Requires `.naming.toml` + sources already on disk.
/// `expected_members` is a sanity hint for the caller; the actual index is
/// built by tagpath's own walker.
fn write_fresh_tagpath_index(root: &Path, expected_members: &[(&str, &str)]) {
    fs::write(
        root.join(".naming.toml"),
        r#"version = 1
name = "tsift-path-test"
convention = "snake_case"

[contexts.function]
convention = "snake_case"

[contexts.type]
convention = "PascalCase"
"#,
    )
    .unwrap();
    let index = tagpath::index::build(&tagpath::index::BuildOptions {
        project_root: root.to_path_buf(),
    })
    .expect("tagpath build");
    let idx_path = tagpath::index::index_path(root);
    fs::create_dir_all(idx_path.parent().unwrap()).unwrap();
    tagpath::index::write(&index, &idx_path).expect("tagpath write");
    for (name, file) in expected_members {
        let found = index
            .families
            .iter()
            .flat_map(|f| f.members.iter())
            .any(|m| m.name == *name && m.path.ends_with(file));
        assert!(
            found,
            "tagpath fixture missing member ({name}, {file}); families={:?}",
            index.families
        );
    }
}

fn tagpath_member_handle(root: &Path, name: &str, file: &str) -> String {
    let idx_path = tagpath::index::index_path(root);
    let index = tagpath::index::read(&idx_path).expect("tagpath read");
    index
        .families
        .iter()
        .flat_map(|f| f.members.iter())
        .find(|m| m.name == name && m.path.ends_with(file))
        .unwrap_or_else(|| panic!("tagpath fixture missing handle for ({name}, {file})"))
        .handle
        .clone()
}

fn indexed_cli_fixture() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    build_cli_fixture(dir.path());

    let output = tsift_bin()
        .args(["index", dir.path().to_str().unwrap()])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "index stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    dir
}

fn git_indexed_cli_fixture() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    build_cli_fixture(dir.path());
    init_git_repo(dir.path());

    let output = tsift_bin()
        .args(["index", dir.path().to_str().unwrap()])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "index stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    dir
}

fn write_ast_cst_rust_edit_fixture(path: &Path) {
    fs::write(
        path.join("main.rs"),
        r#"#![allow(dead_code)]

use std::io;

// Keep the module banner comment.

// <generated:do-not-edit>
macro_rules! make_value {
    () => {
        41
    };
}
// </generated:do-not-edit>

fn alpha() {
    // Keep the local comment until a body replacement owns this range.
    let value = make_value!();
    println!("value: {value}");
}

// Keep beta call comment.
fn beta() {
    alpha();
}
"#,
    )
    .unwrap();
}

fn ast_cst_rust_edit_fixture() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    write_ast_cst_rust_edit_fixture(dir.path());

    let output = tsift_bin()
        .args(["index", dir.path().to_str().unwrap()])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "index stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    dir
}

fn git_ast_cst_rust_edit_fixture() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    write_ast_cst_rust_edit_fixture(dir.path());
    init_git_repo(dir.path());

    let output = tsift_bin()
        .args(["index", dir.path().to_str().unwrap()])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "index stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    dir
}

fn structural_edit_fixture() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    fs::write(
        dir.path().join("main.rs"),
        r#"pub struct Widget {
    value: i32,
}

pub fn moved() -> i32 {
    7
}

fn caller() -> i32 {
    moved()
}
"#,
    )
    .unwrap();
    fs::write(
        dir.path().join("widget.rs"),
        r#"pub fn existing() -> i32 {
    1
}
"#,
    )
    .unwrap();

    let output = tsift_bin()
        .args(["index", dir.path().to_str().unwrap()])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "index stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    dir
}

fn script_edit_fixture() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    fs::write(
        dir.path().join("tool.ts"),
        r#"import { base } from "./base";

function alpha(value: number): number {
  return beta(value);
}

function beta(value: number): number {
  return value + 1;
}
"#,
    )
    .unwrap();
    fs::write(
        dir.path().join("app.js"),
        r#"function alpha(value) {
  return beta(value);
}

function beta(value) {
  return value + 1;
}
"#,
    )
    .unwrap();
    fs::write(
        dir.path().join("script.py"),
        r#"import os

def alpha(value):
    return beta(value)

def beta(value):
    return value + 1
"#,
    )
    .unwrap();

    let output = tsift_bin()
        .args(["index", dir.path().to_str().unwrap()])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "index stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    dir
}

fn mixed_language_markdown_edit_fixture() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    fs::write(
        dir.path().join("README.md"),
        r#"# Mixed Blocks

## Usage

```rust
fn sample() {}
```

```ts
function sample() {
  return 1;
}
```

```python
def sample():
    return 1
```
"#,
    )
    .unwrap();

    let output = tsift_bin()
        .args(["index", dir.path().to_str().unwrap()])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "index stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    dir
}

fn write_markdown_edit_fixture(path: &Path) {
    fs::write(
        path.join("README.md"),
        r#"# Guide

Intro text.

## Install

- Run setup.
  - Confirm setup.

```rust
fn sample() {}
```

### Troubleshooting

Check logs.

## Reference

Done.
"#,
    )
    .unwrap();
}

fn markdown_edit_fixture() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    write_markdown_edit_fixture(dir.path());

    let output = tsift_bin()
        .args(["index", dir.path().to_str().unwrap()])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "index stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    dir
}

fn git_markdown_edit_fixture() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    write_markdown_edit_fixture(dir.path());
    init_git_repo(dir.path());

    let output = tsift_bin()
        .args(["index", dir.path().to_str().unwrap()])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "index stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    dir
}

fn setup_tokensave_db(dir: &Path) {
    let tokensave_dir = dir.join(".tokensave");
    fs::create_dir_all(&tokensave_dir).unwrap();
    let db_path = tokensave_dir.join("tokensave.db");
    let conn = Connection::open(&db_path).unwrap();
    conn.execute_batch(
        r#"
        CREATE TABLE nodes (
            id TEXT PRIMARY KEY,
            kind TEXT NOT NULL,
            name TEXT NOT NULL,
            qualified_name TEXT NOT NULL,
            file_path TEXT NOT NULL,
            start_line INTEGER NOT NULL,
            end_line INTEGER NOT NULL,
            start_column INTEGER NOT NULL DEFAULT 0,
            end_column INTEGER NOT NULL DEFAULT 0,
            docstring TEXT,
            signature TEXT,
            visibility TEXT NOT NULL DEFAULT 'private',
            is_async INTEGER NOT NULL DEFAULT 0,
            branches INTEGER NOT NULL DEFAULT 0,
            loops INTEGER NOT NULL DEFAULT 0,
            returns INTEGER NOT NULL DEFAULT 0,
            max_nesting INTEGER NOT NULL DEFAULT 0,
            unsafe_blocks INTEGER NOT NULL DEFAULT 0,
            unchecked_calls INTEGER NOT NULL DEFAULT 0,
            assertions INTEGER NOT NULL DEFAULT 0,
            updated_at INTEGER NOT NULL DEFAULT 0,
            attrs_start_line INTEGER NOT NULL DEFAULT 0,
            parent_id TEXT
        );
        CREATE TABLE edges (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            source TEXT NOT NULL,
            target TEXT NOT NULL,
            kind TEXT NOT NULL,
            line INTEGER
        );
        CREATE TABLE files (
            path TEXT PRIMARY KEY,
            content_hash TEXT NOT NULL,
            size INTEGER NOT NULL,
            modified_at INTEGER NOT NULL,
            indexed_at INTEGER NOT NULL,
            node_count INTEGER NOT NULL DEFAULT 0
        );
        CREATE INDEX idx_nodes_kind ON nodes(kind);
        CREATE INDEX idx_edges_source_kind ON edges(source, kind);
        CREATE INDEX idx_edges_target_kind ON edges(target, kind);
        "#,
    )
    .unwrap();
    conn.execute(
        "INSERT INTO nodes (id, kind, name, qualified_name, file_path, start_line, end_line) \
         VALUES ('fn:main', 'function', 'main', 'main', 'src/main.rs', 1, 8)",
        [],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO nodes (id, kind, name, qualified_name, file_path, start_line, end_line) \
         VALUES ('fn:helper', 'function', 'helper', 'helper', 'src/lib.rs', 2, 4)",
        [],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO edges (source, target, kind, line) VALUES ('fn:main', 'fn:helper', 'calls', 3)",
        [],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO files (path, content_hash, size, modified_at, indexed_at, node_count) \
         VALUES ('src/main.rs', 'abc123', 128, 1000, 1001, 1)",
        [],
    )
    .unwrap();
}

fn indexed_workspace_cli_fixture() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    fs::write(
        dir.path().join(".gitmodules"),
        r#"[submodule "src/alpha"]
	path = src/alpha
	url = https://example.com/alpha
[submodule "src/beta"]
	path = src/beta
	url = https://example.com/beta
"#,
    )
    .unwrap();
    fs::create_dir_all(dir.path().join("src/alpha")).unwrap();
    fs::create_dir_all(dir.path().join("src/beta")).unwrap();
    fs::write(
        dir.path().join("src/alpha/lib.rs"),
        "fn alpha_helper() {}\nfn alpha_main() { alpha_helper(); }\n",
    )
    .unwrap();
    fs::write(dir.path().join("src/beta/lib.rs"), "fn beta_func() {}\n").unwrap();

    let output = tsift_bin()
        .args(["index", "--workspace", dir.path().to_str().unwrap()])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "workspace index stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    dir
}

fn create_summary_cache(dir: &Path) {
    fs::create_dir_all(dir.join(".tsift")).unwrap();
    let db_path = dir.join(".tsift/summaries.db");
    let conn = Connection::open(&db_path).unwrap();
    conn.execute_batch(
        "PRAGMA journal_mode=WAL;
         CREATE TABLE summaries (
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
         CREATE INDEX idx_summaries_symbol ON summaries(symbol_name);
         CREATE INDEX idx_summaries_file ON summaries(file_path);
         CREATE INDEX idx_summaries_hash ON summaries(content_hash);",
    )
    .unwrap();
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
            "alpha_helper",
            "src/lib.rs",
            "hash1",
            "cached summary",
            Option::<String>::None,
            Option::<String>::None,
            Option::<String>::None,
            "1700000000",
            "claude-haiku-4-5-20251001",
            100_i64,
            40_i64
        ],
    )
    .unwrap();
}

fn mock_anthropic_extraction(command: &mut Command) -> &mut Command {
    command.env("ANTHROPIC_API_KEY", "test-key").env(
        "TSIFT_TEST_ANTHROPIC_RESPONSE_JSON",
        r#"{"summary":"ok","entities":[],"relationships":[],"concept_labels":[]}"#,
    )
}

#[test]
fn check_exit_code_zero_when_fresh() {
    let dir = tempfile::tempdir().unwrap();
    fs::write(dir.path().join("main.rs"), "fn main() {}").unwrap();

    // Index first
    let status = tsift_bin()
        .args(["index", dir.path().to_str().unwrap()])
        .status()
        .unwrap();
    assert!(status.success());

    // Check with --exit-code should exit 0 (no stale files)
    let status = tsift_bin()
        .args([
            "index",
            "--check",
            "--exit-code",
            dir.path().to_str().unwrap(),
        ])
        .status()
        .unwrap();
    assert!(status.success(), "expected exit 0 for fresh index");
}

#[test]
fn check_exit_code_one_when_stale() {
    let dir = tempfile::tempdir().unwrap();
    fs::write(dir.path().join("main.rs"), "fn main() {}").unwrap();

    // Index first
    tsift_bin()
        .args(["index", dir.path().to_str().unwrap()])
        .status()
        .unwrap();

    // Add a new file to make the index stale
    fs::write(dir.path().join("lib.rs"), "fn helper() {}").unwrap();

    // Check with --exit-code should exit 1 (stale files exist)
    let status = tsift_bin()
        .args([
            "index",
            "--check",
            "--exit-code",
            dir.path().to_str().unwrap(),
        ])
        .status()
        .unwrap();
    assert!(!status.success(), "expected exit 1 for stale index");
}

#[test]
fn check_without_exit_code_always_zero() {
    let dir = tempfile::tempdir().unwrap();
    fs::write(dir.path().join("main.rs"), "fn main() {}").unwrap();

    // Index first
    tsift_bin()
        .args(["index", dir.path().to_str().unwrap()])
        .status()
        .unwrap();

    // Add new file
    fs::write(dir.path().join("lib.rs"), "fn helper() {}").unwrap();

    // Check without --exit-code should still exit 0
    let status = tsift_bin()
        .args(["index", "--check", dir.path().to_str().unwrap()])
        .status()
        .unwrap();
    assert!(
        status.success(),
        "expected exit 0 when --exit-code not specified"
    );
}

#[test]
fn check_exit_code_one_when_modified() {
    let dir = tempfile::tempdir().unwrap();
    fs::write(dir.path().join("main.rs"), "fn main() {}").unwrap();

    tsift_bin()
        .args(["index", dir.path().to_str().unwrap()])
        .status()
        .unwrap();

    std::thread::sleep(std::time::Duration::from_millis(50));
    fs::write(
        dir.path().join("main.rs"),
        "fn main() { println!(\"hi\"); }",
    )
    .unwrap();

    let status = tsift_bin()
        .args([
            "index",
            "--check",
            "--exit-code",
            dir.path().to_str().unwrap(),
        ])
        .status()
        .unwrap();
    assert!(!status.success(), "expected exit 1 for modified file");
}

#[test]
fn check_exit_code_one_when_deleted() {
    let dir = tempfile::tempdir().unwrap();
    fs::write(dir.path().join("main.rs"), "fn main() {}").unwrap();
    fs::write(dir.path().join("lib.rs"), "fn helper() {}").unwrap();

    tsift_bin()
        .args(["index", dir.path().to_str().unwrap()])
        .status()
        .unwrap();

    fs::remove_file(dir.path().join("lib.rs")).unwrap();

    let status = tsift_bin()
        .args([
            "index",
            "--check",
            "--exit-code",
            dir.path().to_str().unwrap(),
        ])
        .status()
        .unwrap();
    assert!(!status.success(), "expected exit 1 for deleted file");
}

#[test]
fn check_exit_code_zero_when_no_index_exists() {
    let dir = tempfile::tempdir().unwrap();
    fs::write(dir.path().join("main.rs"), "fn main() {}").unwrap();

    // No prior index — all files are "new", so --check --exit-code should exit 1
    let status = tsift_bin()
        .args([
            "index",
            "--check",
            "--exit-code",
            dir.path().to_str().unwrap(),
        ])
        .status()
        .unwrap();
    assert!(
        !status.success(),
        "expected exit 1 when no index exists (all files are new)"
    );
}

#[test]
fn search_autoindexes_stale_index_by_default() {
    let dir = tempfile::tempdir().unwrap();
    fs::write(dir.path().join("main.rs"), "fn main() {}").unwrap();

    let status = tsift_bin()
        .args(["index", dir.path().to_str().unwrap()])
        .status()
        .unwrap();
    assert!(status.success());

    std::thread::sleep(std::time::Duration::from_millis(50));
    fs::write(
        dir.path().join("main.rs"),
        "fn helper() {}\nfn main() { helper(); }",
    )
    .unwrap();

    let output = tsift_bin()
        .args(["search", "--path", dir.path().to_str().unwrap(), "helper"])
        .output()
        .unwrap();

    assert!(
        output.status.success(),
        "expected default search to autoindex"
    );
}

#[test]
fn search_json_reports_symbol_and_content_hits() {
    let dir = indexed_cli_fixture();

    let output = tsift_bin()
        .args([
            "search",
            "--path",
            dir.path().to_str().unwrap(),
            "--json",
            "alpha",
        ])
        .output()
        .unwrap();

    assert!(output.status.success(), "search should succeed");
    let json: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();

    let symbols = json["symbols"].as_array().unwrap();
    assert!(
        symbols.iter().any(|sym| sym["name"] == "alpha"),
        "expected alpha in symbol hits: {json}"
    );

    let hits = json["hits"].as_array().unwrap();
    assert!(
        !hits.is_empty(),
        "expected lexical content hits for alpha: {json}"
    );
    assert!(
        hits.iter()
            .any(|hit| hit["path"].as_str().is_some_and(|path| path == "main.rs")),
        "expected main.rs in lexical hits: {json}"
    );
}

#[test]
fn graph_json_reports_callers_and_callees() {
    let dir = indexed_cli_fixture();

    let output = tsift_bin()
        .args(["graph", "alpha", dir.path().to_str().unwrap(), "--json"])
        .output()
        .unwrap();

    assert!(output.status.success(), "graph should succeed");
    let json: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();

    assert_eq!(json["symbol"], "alpha");
    assert_eq!(json["callers_total"].as_u64(), Some(3));
    assert_eq!(json["callees_total"].as_u64(), Some(2));

    let callers = json["callers"].as_array().unwrap();
    assert!(
        callers
            .iter()
            .any(|edge| edge["caller_name"] == "main" && edge["caller_file"] == "main.rs")
    );
    let callees = json["callees"].as_array().unwrap();
    assert!(callees.iter().any(|edge| edge["callee_name"] == "beta"));
    assert!(callees.iter().any(|edge| edge["callee_name"] == "gamma"));
    // Without a tagpath index in the fixture, edges stay handle-free.
    for caller in callers {
        assert!(caller.get("tagpath_handle").is_none(), "{caller}");
    }
    for callee in callees {
        assert!(callee.get("tagpath_handle").is_none(), "{callee}");
    }
}

#[test]
fn analyze_json_runs_graph_algorithms() {
    let dir = indexed_cli_fixture();

    let output = tsift_bin()
        .args([
            "analyze",
            dir.path().to_str().unwrap(),
            "--entry",
            "main",
            "--json",
        ])
        .output()
        .unwrap();

    assert!(
        output.status.success(),
        "analyze stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let json: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();

    assert_eq!(json["edge_count"].as_u64(), Some(12));
    assert_eq!(json["entry_points"], serde_json::json!(["main"]));
    assert!(
        json["scc"]["non_trivial_count"].as_u64().unwrap_or(0) >= 2,
        "expected cycle analysis in scc report: {json}"
    );
    assert!(
        json["health"]["avg_overall"].as_f64().is_some(),
        "expected health report: {json}"
    );
    assert!(
        json["dead_code"]["dead_count"].as_u64().unwrap_or(0) >= 2,
        "expected disconnected delta/epsilon cycle as dead code: {json}"
    );
    assert!(
        json["coupling"]["total_modules"].as_u64().unwrap_or(0) >= 1,
        "expected coupling report: {json}"
    );
}

#[test]
fn graph_db_tokensave_backend_queries_tokensave_db() {
    let dir = tempfile::tempdir().unwrap();
    setup_tokensave_db(dir.path());

    let output = tsift_bin()
        .args([
            "graph-db",
            "--path",
            dir.path().to_str().unwrap(),
            "--backend",
            "tokensave",
            "--json",
            "node",
            "fn:main",
        ])
        .output()
        .unwrap();

    assert!(
        output.status.success(),
        "graph-db tokensave stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let json: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();

    assert_eq!(json["backend"], "tokensave");
    assert_eq!(json["freshness"]["status"], "current");
    assert_eq!(json["node"]["id"], "fn:main");
    assert_eq!(json["node"]["kind"], "function");
}

#[test]
fn graph_json_omits_handles_when_no_tagpath_flag_set() {
    let dir = indexed_cli_fixture();
    write_fresh_tagpath_index(dir.path(), &[("alpha", "main.rs"), ("beta", "main.rs")]);

    let output = tsift_bin()
        .args([
            "graph",
            "alpha",
            dir.path().to_str().unwrap(),
            "--json",
            "--no-tagpath",
        ])
        .output()
        .unwrap();

    assert!(output.status.success(), "graph should succeed");
    let json: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    for caller in json["callers"].as_array().unwrap() {
        assert!(
            caller.get("tagpath_handle").is_none(),
            "--no-tagpath should suppress caller handles: {caller}"
        );
    }
    for callee in json["callees"].as_array().unwrap() {
        assert!(
            callee.get("tagpath_handle").is_none(),
            "--no-tagpath should suppress callee handles: {callee}"
        );
    }
}

#[test]
fn graph_json_annotates_caller_and_callee_edges_when_index_is_fresh() {
    let dir = indexed_cli_fixture();
    let members: Vec<(&str, &str)> = vec![
        ("main", "main.rs"),
        ("alpha", "main.rs"),
        ("beta", "main.rs"),
        ("gamma", "main.rs"),
    ];
    write_fresh_tagpath_index(dir.path(), &members);

    let output = tsift_bin()
        .args(["graph", "alpha", dir.path().to_str().unwrap(), "--json"])
        .output()
        .unwrap();

    assert!(
        output.status.success(),
        "graph should succeed (stderr={})",
        String::from_utf8_lossy(&output.stderr)
    );
    let json: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();

    let callers = json["callers"].as_array().unwrap();
    assert!(!callers.is_empty());
    for caller in callers {
        let caller_name = caller["caller_name"].as_str().unwrap();
        let handle = caller["tagpath_handle"]
            .as_str()
            .unwrap_or_else(|| panic!("caller {caller_name} missing tagpath_handle"));
        assert!(handle.starts_with("mem:"), "{caller_name}: {handle}");
    }
    let callees = json["callees"].as_array().unwrap();
    assert!(!callees.is_empty());
    for callee in callees {
        let callee_name = callee["callee_name"].as_str().unwrap();
        let handle = callee["tagpath_handle"]
            .as_str()
            .unwrap_or_else(|| panic!("callee {callee_name} missing tagpath_handle"));
        assert!(handle.starts_with("mem:"), "{callee_name}: {handle}");
    }
}

#[test]
fn graph_callers_only_json_annotates_handle() {
    let dir = indexed_cli_fixture();
    write_fresh_tagpath_index(dir.path(), &[("alpha", "main.rs"), ("main", "main.rs")]);

    let output = tsift_bin()
        .args([
            "graph",
            "alpha",
            dir.path().to_str().unwrap(),
            "--callers",
            "--json",
        ])
        .output()
        .unwrap();

    assert!(output.status.success(), "graph --callers should succeed");
    let json: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    let callers = json["callers"].as_array().unwrap();
    assert!(!callers.is_empty());
    for caller in callers {
        let handle = caller["tagpath_handle"].as_str().unwrap();
        assert!(handle.starts_with("mem:"), "{caller}");
    }
}

#[test]
fn communities_json_reports_disconnected_clusters() {
    let dir = indexed_cli_fixture();

    let output = tsift_bin()
        .args(["communities", dir.path().to_str().unwrap(), "--json"])
        .output()
        .unwrap();

    assert!(output.status.success(), "communities should succeed");
    let json: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();

    assert!(
        json["community_count"].as_u64().unwrap_or(0) >= 2,
        "expected at least two communities: {json}"
    );

    let communities = json["communities"].as_array().unwrap();
    let community_member_names = |c: &serde_json::Value| -> Vec<String> {
        c["members"]
            .as_array()
            .unwrap()
            .iter()
            .map(|m| m["name"].as_str().unwrap().to_string())
            .collect()
    };
    assert!(
        communities.iter().any(|community| {
            let names = community_member_names(community);
            names.contains(&"alpha".to_string())
                && names.contains(&"beta".to_string())
                && names.contains(&"gamma".to_string())
        }),
        "expected alpha/beta/gamma community: {json}"
    );
    assert!(
        communities.iter().any(|community| {
            let names = community_member_names(community);
            names.contains(&"delta".to_string()) && names.contains(&"epsilon".to_string())
        }),
        "expected delta/epsilon community: {json}"
    );
    // Without a tagpath index in the fixture, no member should carry a handle.
    for community in communities {
        for member in community["members"].as_array().unwrap() {
            assert!(
                member.get("tagpath_handle").is_none(),
                "unexpected handle: {member}"
            );
        }
    }
}

#[test]
fn communities_json_omits_handles_when_no_tagpath_flag_set() {
    let dir = indexed_cli_fixture();
    write_fresh_tagpath_index(dir.path(), &[("alpha", "main.rs"), ("beta", "main.rs")]);

    let output = tsift_bin()
        .args([
            "communities",
            dir.path().to_str().unwrap(),
            "--json",
            "--no-tagpath",
        ])
        .output()
        .unwrap();

    assert!(output.status.success(), "communities should succeed");
    let json: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    for community in json["communities"].as_array().unwrap() {
        for member in community["members"].as_array().unwrap() {
            assert!(
                member.get("tagpath_handle").is_none(),
                "--no-tagpath should suppress handles: {member}"
            );
        }
    }
}

#[test]
fn communities_json_annotates_members_when_index_is_fresh() {
    let dir = indexed_cli_fixture();
    let members: Vec<(&str, &str)> = vec![
        ("alpha", "main.rs"),
        ("beta", "main.rs"),
        ("gamma", "main.rs"),
        ("delta", "main.rs"),
        ("epsilon", "main.rs"),
    ];
    write_fresh_tagpath_index(dir.path(), &members);

    let output = tsift_bin()
        .args(["communities", dir.path().to_str().unwrap(), "--json"])
        .output()
        .unwrap();

    assert!(
        output.status.success(),
        "communities should succeed (stderr={})",
        String::from_utf8_lossy(&output.stderr)
    );
    let json: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    let diag = &json["community_diagnostics"];
    assert_eq!(diag["tagpath_state"], "fresh", "{json}");
    assert_eq!(diag["tagpath_readiness"]["status"], "ready", "{json}");
    assert_eq!(diag["tagpath_readiness"]["fail_closed"], false, "{json}");
    let communities = json["communities"].as_array().unwrap();
    assert!(!communities.is_empty());
    for community in communities {
        for member in community["members"].as_array().unwrap() {
            let name = member["name"].as_str().unwrap();
            let handle = member["tagpath_handle"]
                .as_str()
                .unwrap_or_else(|| panic!("community member {name} missing tagpath_handle"));
            assert!(handle.starts_with("mem:"), "{name}: {handle}");
        }
    }
}

#[test]
fn communities_json_reports_cache_diagnostics_and_reuses_disk_cache() {
    let dir = indexed_cli_fixture();

    let first = tsift_bin()
        .args(["communities", dir.path().to_str().unwrap(), "--json"])
        .output()
        .unwrap();
    assert!(
        first.status.success(),
        "first communities run should succeed (stderr={})",
        String::from_utf8_lossy(&first.stderr)
    );
    let first_json: serde_json::Value = serde_json::from_slice(&first.stdout).unwrap();
    let first_diag = &first_json["community_diagnostics"];
    assert_eq!(first_diag["cache_hit"], false, "{first_json}");
    assert_eq!(first_diag["tagpath_state"], "missing", "{first_json}");
    assert_eq!(
        first_diag["tagpath_readiness"]["status"], "blocked",
        "{first_json}"
    );
    assert_eq!(
        first_diag["tagpath_readiness"]["fail_closed"], true,
        "{first_json}"
    );
    assert_eq!(
        first_diag["tagpath_readiness"]["reason"], "tagpath_state_missing",
        "{first_json}"
    );
    assert!(
        first_diag["tagpath_readiness"]["next_commands"]
            .as_array()
            .unwrap()
            .iter()
            .any(|command| command.as_str().unwrap().contains("tagpath index")),
        "{first_json}"
    );
    assert_eq!(
        first_diag["edge_count"], first_json["edge_count"],
        "{first_json}"
    );
    assert_eq!(
        first_diag["iterations"], first_json["iterations"],
        "{first_json}"
    );
    assert_eq!(first_diag["annotated_member_count"], 0, "{first_json}");

    let second = tsift_bin()
        .args(["communities", dir.path().to_str().unwrap(), "--json"])
        .output()
        .unwrap();
    assert!(
        second.status.success(),
        "second communities run should succeed (stderr={})",
        String::from_utf8_lossy(&second.stderr)
    );
    let second_json: serde_json::Value = serde_json::from_slice(&second.stdout).unwrap();
    assert_eq!(
        second_json["community_diagnostics"]["cache_hit"], true,
        "{second_json}"
    );
}

#[test]
fn communities_json_bounds_tagpath_annotation_to_displayed_results() {
    let dir = indexed_cli_fixture();
    let members: Vec<(&str, &str)> = vec![
        ("main", "main.rs"),
        ("alpha", "main.rs"),
        ("beta", "main.rs"),
        ("gamma", "main.rs"),
        ("bridge", "main.rs"),
        ("shared", "main.rs"),
        ("helper", "main.rs"),
        ("delta", "main.rs"),
        ("epsilon", "main.rs"),
    ];
    write_fresh_tagpath_index(dir.path(), &members);

    let output = tsift_bin()
        .args([
            "communities",
            dir.path().to_str().unwrap(),
            "--json",
            "--limit",
            "1",
        ])
        .output()
        .unwrap();

    assert!(
        output.status.success(),
        "communities should succeed (stderr={})",
        String::from_utf8_lossy(&output.stderr)
    );
    let json: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    let communities = json["communities"].as_array().unwrap();
    assert_eq!(communities.len(), 1, "{json}");
    let displayed_members = communities[0]["members"].as_array().unwrap().len() as u64;
    let diag = &json["community_diagnostics"];
    assert_eq!(diag["tagpath_state"], "fresh", "{json}");
    assert_eq!(diag["annotated_community_count"], 1, "{json}");
    assert_eq!(diag["annotated_member_count"], displayed_members, "{json}");
}

// Regression: federated search must annotate each hit against its own
// scope's tagpath project rather than the workspace root, which usually has
// no `.naming.toml`. Each submodule below installs its own tagpath index;
// `tsift search --federated` should pick up handles from both.
#[test]
fn search_federated_json_annotates_handles_from_per_scope_tagpath_indexes() {
    let dir = tempfile::tempdir().unwrap();
    fs::write(
        dir.path().join(".gitmodules"),
        r#"[submodule "src/alpha"]
	path = src/alpha
	url = https://example.com/alpha
[submodule "src/beta"]
	path = src/beta
	url = https://example.com/beta
"#,
    )
    .unwrap();

    for scope in ["alpha", "beta"] {
        let scope_root = dir.path().join(format!("src/{scope}"));
        fs::create_dir_all(&scope_root).unwrap();
        fs::write(
            scope_root.join("lib.rs"),
            "fn shared_helper() {}\nfn local_caller() { shared_helper(); }\n",
        )
        .unwrap();
        fs::write(
            scope_root.join(".naming.toml"),
            r#"version = 1
name = "scope-test"
convention = "snake_case"

[contexts.function]
convention = "snake_case"

[contexts.type]
convention = "PascalCase"
"#,
        )
        .unwrap();
        let index = tagpath::index::build(&tagpath::index::BuildOptions {
            project_root: scope_root.clone(),
        })
        .expect("tagpath build");
        let idx_path = tagpath::index::index_path(&scope_root);
        fs::create_dir_all(idx_path.parent().unwrap()).unwrap();
        tagpath::index::write(&index, &idx_path).expect("tagpath write");
    }

    let output = tsift_bin()
        .args(["index", "--workspace", dir.path().to_str().unwrap()])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "workspace index stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let output = tsift_bin()
        .args([
            "search",
            "--path",
            dir.path().to_str().unwrap(),
            "--federated",
            "--json",
            "shared_helper",
        ])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "search stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let json: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();

    let symbols = json["symbols"].as_array().expect("symbols array");
    let mut alpha_handle: Option<String> = None;
    let mut beta_handle: Option<String> = None;
    for sym in symbols {
        if sym["name"].as_str() != Some("shared_helper") {
            continue;
        }
        let file = sym["file"].as_str().unwrap_or("");
        let handle = sym["tagpath_handle"]
            .as_str()
            .unwrap_or_else(|| panic!("federated hit `{file}` missing tagpath_handle: {sym}"));
        assert!(handle.starts_with("mem:"), "{handle}");
        if file.contains("alpha") {
            alpha_handle = Some(handle.to_string());
        }
        if file.contains("beta") {
            beta_handle = Some(handle.to_string());
        }
    }
    assert!(
        alpha_handle.is_some(),
        "missing alpha-scope tagpath_handle: {json}"
    );
    assert!(
        beta_handle.is_some(),
        "missing beta-scope tagpath_handle: {json}"
    );
}

// Regression: scoped search must annotate against the scope's source_root
// (where the submodule's `.naming.toml` lives), not the workspace root.
// Mirror of the federated bug closed in 0.1.57 (#p6tsifullfederated).
#[test]
fn search_scoped_json_annotates_handles_from_submodule_tagpath() {
    let dir = tempfile::tempdir().unwrap();
    fs::write(
        dir.path().join(".gitmodules"),
        r#"[submodule "src/alpha"]
	path = src/alpha
	url = https://example.com/alpha
"#,
    )
    .unwrap();
    let scope_root = dir.path().join("src/alpha");
    fs::create_dir_all(&scope_root).unwrap();
    fs::write(
        scope_root.join("lib.rs"),
        "fn scoped_helper() {}\nfn caller() { scoped_helper(); }\n",
    )
    .unwrap();
    fs::write(
        scope_root.join(".naming.toml"),
        r#"version = 1
name = "scope-test"
convention = "snake_case"

[contexts.function]
convention = "snake_case"

[contexts.type]
convention = "PascalCase"
"#,
    )
    .unwrap();
    let index = tagpath::index::build(&tagpath::index::BuildOptions {
        project_root: scope_root.clone(),
    })
    .expect("tagpath build");
    let idx_path = tagpath::index::index_path(&scope_root);
    fs::create_dir_all(idx_path.parent().unwrap()).unwrap();
    tagpath::index::write(&index, &idx_path).expect("tagpath write");

    // The workspace root has no `.naming.toml`; before the fix the
    // annotation walks up from `&root` (workspace) and returns Missing,
    // dropping the handle even though the submodule has a valid index.
    let output = tsift_bin()
        .args(["index", "--workspace", dir.path().to_str().unwrap()])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "tsift workspace index stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let output = tsift_bin()
        .args([
            "search",
            "--path",
            dir.path().to_str().unwrap(),
            "--scope",
            "alpha",
            "--json",
            "scoped_helper",
        ])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "scoped search stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let json: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    let symbols = json["symbols"].as_array().expect("symbols array");
    let scoped = symbols
        .iter()
        .find(|s| s["name"].as_str() == Some("scoped_helper"))
        .unwrap_or_else(|| panic!("expected scoped_helper hit: {json}"));
    let handle = scoped["tagpath_handle"]
        .as_str()
        .unwrap_or_else(|| panic!("scoped_helper missing tagpath_handle: {scoped}"));
    assert!(handle.starts_with("mem:"), "{handle}");
}

// `tsift audit-tagpath --scope <name>` scopes the walker diff to a single
// submodule. Implementation routes through `config::Config::resolve_submodule`
// + scope.source_root; this test locks in that path so a regression cannot
// silently fall back to the workspace root.
#[test]
fn audit_tagpath_scope_reports_per_submodule_walker_diff() {
    let dir = tempfile::tempdir().unwrap();
    fs::write(
        dir.path().join(".gitmodules"),
        r#"[submodule "src/alpha"]
	path = src/alpha
	url = https://example.com/alpha
[submodule "src/beta"]
	path = src/beta
	url = https://example.com/beta
"#,
    )
    .unwrap();

    // alpha has a `__pycache__/` file tsift indexes but tagpath skips.
    let alpha_root = dir.path().join("src/alpha");
    fs::create_dir_all(alpha_root.join("__pycache__")).unwrap();
    fs::write(
        alpha_root.join("__pycache__/lib.rs"),
        "fn cached_alpha() {}\n",
    )
    .unwrap();
    fs::write(
        alpha_root.join("lib.rs"),
        "fn alpha_helper() {}\nfn alpha_caller() { alpha_helper(); }\n",
    )
    .unwrap();
    fs::write(
        alpha_root.join(".naming.toml"),
        r#"version = 1
name = "alpha-scope"
convention = "snake_case"

[contexts.function]
convention = "snake_case"

[contexts.type]
convention = "PascalCase"
"#,
    )
    .unwrap();
    let alpha_index = tagpath::index::build(&tagpath::index::BuildOptions {
        project_root: alpha_root.clone(),
    })
    .expect("tagpath build alpha");
    let alpha_idx_path = tagpath::index::index_path(&alpha_root);
    fs::create_dir_all(alpha_idx_path.parent().unwrap()).unwrap();
    tagpath::index::write(&alpha_index, &alpha_idx_path).expect("tagpath write alpha");

    // beta has a clean, fully-covered source set (no walker diff).
    let beta_root = dir.path().join("src/beta");
    fs::create_dir_all(&beta_root).unwrap();
    fs::write(beta_root.join("lib.rs"), "fn beta_helper() {}\n").unwrap();
    fs::write(
        beta_root.join(".naming.toml"),
        r#"version = 1
name = "beta-scope"
convention = "snake_case"

[contexts.function]
convention = "snake_case"

[contexts.type]
convention = "PascalCase"
"#,
    )
    .unwrap();
    let beta_index = tagpath::index::build(&tagpath::index::BuildOptions {
        project_root: beta_root.clone(),
    })
    .expect("tagpath build beta");
    let beta_idx_path = tagpath::index::index_path(&beta_root);
    fs::create_dir_all(beta_idx_path.parent().unwrap()).unwrap();
    tagpath::index::write(&beta_index, &beta_idx_path).expect("tagpath write beta");

    let output = tsift_bin()
        .args(["index", "--workspace", dir.path().to_str().unwrap()])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "workspace index stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    // Audit alpha: should report the __pycache__ diff inside alpha only.
    let output = tsift_bin()
        .args([
            "audit-tagpath",
            "--path",
            dir.path().to_str().unwrap(),
            "--scope",
            "alpha",
            "--json",
        ])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "audit alpha stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let json: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(
        json["scope"].as_str(),
        Some("alpha"),
        "expected scope=alpha in JSON: {json}"
    );
    assert_eq!(json["tagpath_state"], "fresh", "{json}");

    let tsift_only = json["tsift_only_files"]
        .as_array()
        .expect("tsift_only_files array")
        .iter()
        .filter_map(|v| v.as_str())
        .map(str::to_string)
        .collect::<Vec<_>>();
    assert!(
        tsift_only.iter().any(|f| f.contains("__pycache__/lib.rs")),
        "expected alpha's __pycache__/lib.rs in tsift-only list: {json}"
    );
    // The scoped audit must NOT report files from other submodules.
    assert!(
        !tsift_only.iter().any(|f| f.contains("beta")),
        "alpha-scoped audit leaked beta files: {json}"
    );

    // Audit beta: should be clean (no walker diff).
    let output = tsift_bin()
        .args([
            "audit-tagpath",
            "--path",
            dir.path().to_str().unwrap(),
            "--scope",
            "beta",
            "--json",
        ])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "audit beta stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let json: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(json["scope"].as_str(), Some("beta"), "{json}");
    assert_eq!(
        json["tsift_only_files"].as_array().map(|a| a.len()),
        Some(0),
        "beta scope should have no tsift-only files: {json}"
    );
    assert_eq!(
        json["tagpath_only_files"].as_array().map(|a| a.len()),
        Some(0),
        "beta scope should have no tagpath-only files: {json}"
    );
}

// `tsift audit-tagpath` reports files covered by one walker but not the
// other so operators can decide whether to broaden tagpath, narrow tsift,
// or accept the gap. Tagpath skips `__pycache__/` via SKIP_DIRS but tsift
// indexes it, so a `.rs` file inside `__pycache__/` is a textbook
// tsift-only file. Conversely, tagpath sources that no longer exist in
// tsift (e.g. a file extension tsift ignores) show as tagpath-only.
#[test]
fn audit_tagpath_reports_walker_diff() {
    let dir = tempfile::tempdir().unwrap();
    // tsift indexes both files; tagpath skips __pycache__/.
    fs::create_dir_all(dir.path().join("__pycache__")).unwrap();
    fs::write(
        dir.path().join("__pycache__/lib.rs"),
        "fn cached_helper() {}\n",
    )
    .unwrap();
    fs::write(
        dir.path().join("main.rs"),
        "fn helper() {}\nfn caller() { helper(); }\n",
    )
    .unwrap();
    fs::write(
        dir.path().join("stubs.pyi"),
        "def typed_helper() -> None: ...\n",
    )
    .unwrap();

    let output = tsift_bin()
        .args(["index", dir.path().to_str().unwrap()])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "tsift index stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    write_fresh_tagpath_index(dir.path(), &[("helper", "main.rs")]);

    let output = tsift_bin()
        .args([
            "audit-tagpath",
            "--path",
            dir.path().to_str().unwrap(),
            "--json",
        ])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "audit-tagpath stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let json: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(json["tagpath_state"], "fresh", "{json}");

    let tsift_only = json["tsift_only_files"]
        .as_array()
        .expect("tsift_only_files array")
        .iter()
        .filter_map(|v| v.as_str())
        .map(str::to_string)
        .collect::<Vec<_>>();
    assert!(
        tsift_only.iter().any(|f| f.contains("__pycache__/lib.rs")),
        "expected __pycache__/lib.rs in tsift-only list: {json}"
    );
    assert!(
        tsift_only.iter().any(|f| f.contains("stubs.pyi")),
        "expected stubs.pyi in tsift-only list: {json}"
    );

    let unindexed_count = json["tsift_only_symbol_count"]
        .as_u64()
        .expect("tsift_only_symbol_count");
    assert!(
        unindexed_count >= 1,
        "expected at least 1 symbol in tsift-only files: {json}"
    );

    // Files-with-symbols breakdown should list cached_helper's file.
    let files_with_syms = json["tsift_only_files_with_symbols"]
        .as_array()
        .expect("tsift_only_files_with_symbols");
    assert!(
        files_with_syms.iter().any(|entry| {
            entry["file"]
                .as_str()
                .is_some_and(|f| f.contains("__pycache__/lib.rs"))
                && entry["symbols"].as_u64().unwrap_or(0) >= 1
        }),
        "expected __pycache__/lib.rs symbol entry: {json}"
    );

    let policy_hints = json["tsift_only_files_with_policy_hints"]
        .as_array()
        .expect("tsift_only_files_with_policy_hints");
    assert!(
        policy_hints.iter().any(|entry| {
            entry["file"]
                .as_str()
                .is_some_and(|f| f.contains("__pycache__/lib.rs"))
                && entry["hints"]
                    .as_array()
                    .is_some_and(|hints| hints.iter().any(|h| h == "skip_dir:__pycache__"))
        }),
        "expected __pycache__ policy hint: {json}"
    );
    assert!(
        policy_hints.iter().any(|entry| {
            entry["file"]
                .as_str()
                .is_some_and(|f| f.contains("stubs.pyi"))
                && entry["hints"]
                    .as_array()
                    .is_some_and(|hints| hints.iter().any(|h| h == "extension_unsupported"))
        }),
        "expected unsupported extension policy hint: {json}"
    );
}

// Regression: stale-index diagnostic surfaces as top-level JSON fields
// (`tagpath_index_stale: true` + `tagpath_stale_reason`) on every command
// that previously emitted the signal only on stderr, so structured
// consumers can detect the condition without parsing logs.
#[test]
fn json_surfaces_tagpath_stale_diagnostic_when_index_is_stale() {
    let dir = tempfile::tempdir().unwrap();
    fs::write(
        dir.path().join("main.rs"),
        "fn alpha() {}\nfn caller() { alpha(); }\n",
    )
    .unwrap();

    let output = tsift_bin()
        .args(["index", dir.path().to_str().unwrap()])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "tsift index stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    write_fresh_tagpath_index(dir.path(), &[("alpha", "main.rs"), ("caller", "main.rs")]);

    // Mutate source so the tagpath index goes stale (source_modified) while
    // tsift's index stays usable (we want the stale path, not the missing path).
    fs::write(
        dir.path().join("main.rs"),
        "fn alpha() {}\nfn caller() { alpha(); }\nfn extra() {}\n",
    )
    .unwrap();

    // path command: expect both the stderr line AND the new JSON fields.
    let output = tsift_bin()
        .args([
            "path",
            "caller",
            "alpha",
            dir.path().to_str().unwrap(),
            "--json",
        ])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "path stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("tagpath_index_stale: true"),
        "expected stderr stale line: {stderr}"
    );
    let json: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(
        json["tagpath_index_stale"].as_bool(),
        Some(true),
        "expected tagpath_index_stale=true on path JSON: {json}"
    );
    let reason = json["tagpath_stale_reason"]
        .as_str()
        .expect("expected tagpath_stale_reason on path JSON");
    assert!(
        reason.contains("source_modified") || reason.contains("source_added"),
        "unexpected stale reason: {reason}"
    );

    // communities command: same expectation.
    let output = tsift_bin()
        .args(["communities", dir.path().to_str().unwrap(), "--json"])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "communities stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let json: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(
        json["tagpath_index_stale"].as_bool(),
        Some(true),
        "expected tagpath_index_stale=true on communities JSON: {json}"
    );
    assert!(
        json["tagpath_stale_reason"].as_str().is_some(),
        "expected tagpath_stale_reason on communities JSON: {json}"
    );

    // graph command (combined output): same expectation.
    let output = tsift_bin()
        .args(["graph", "alpha", dir.path().to_str().unwrap(), "--json"])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "graph stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let json: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(
        json["tagpath_index_stale"].as_bool(),
        Some(true),
        "expected tagpath_index_stale=true on graph JSON: {json}"
    );

    // explain command: same expectation.
    let output = tsift_bin()
        .args(["explain", "alpha", dir.path().to_str().unwrap(), "--json"])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "explain stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let json: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(
        json["tagpath_index_stale"].as_bool(),
        Some(true),
        "expected tagpath_index_stale=true on explain JSON: {json}"
    );

    // search command (default JSON): same expectation.
    let output = tsift_bin()
        .args([
            "search",
            "alpha",
            "--path",
            dir.path().to_str().unwrap(),
            "--json",
        ])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "search stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let json: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(
        json["tagpath_index_stale"].as_bool(),
        Some(true),
        "expected tagpath_index_stale=true on search JSON: {json}"
    );

    // --no-tagpath should suppress both the stderr line AND the JSON fields.
    let output = tsift_bin()
        .args([
            "path",
            "caller",
            "alpha",
            dir.path().to_str().unwrap(),
            "--json",
            "--no-tagpath",
        ])
        .output()
        .unwrap();
    assert!(output.status.success());
    let json: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert!(
        json.get("tagpath_index_stale").is_none(),
        "--no-tagpath should suppress tagpath_index_stale field: {json}"
    );
}

// Regression: when two files define the same symbol and the first row by
// `(file, line)` lives outside the tagpath index, the resolver must keep
// iterating instead of dropping the handle. `__pycache__/` is in tagpath's
// hard-coded SKIP_DIRS but tsift's `ignore`-based walker still indexes it,
// and `_` (0x5F) sorts before `s` (0x73) so `__pycache__/main.rs` is the
// first row tsift returns for `symbol_info("helper")`.
#[test]
fn communities_json_resolves_handle_through_name_collision() {
    let dir = tempfile::tempdir().unwrap();
    fs::create_dir_all(dir.path().join("__pycache__")).unwrap();
    fs::write(
        dir.path().join("__pycache__/main.rs"),
        "fn helper() {}\nfn vendor_caller() { helper(); }\n",
    )
    .unwrap();
    fs::create_dir_all(dir.path().join("src")).unwrap();
    fs::write(
        dir.path().join("src/main.rs"),
        "fn helper() {}\nfn src_caller() { helper(); }\n",
    )
    .unwrap();

    let output = tsift_bin()
        .args(["index", dir.path().to_str().unwrap()])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "index stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    write_fresh_tagpath_index(dir.path(), &[("helper", "src/main.rs")]);
    let expected = tagpath_member_handle(dir.path(), "helper", "src/main.rs");

    let output = tsift_bin()
        .args(["communities", dir.path().to_str().unwrap(), "--json"])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "communities stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let json: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();

    let mut helper_members: Vec<&serde_json::Value> = Vec::new();
    for community in json["communities"].as_array().unwrap() {
        for member in community["members"].as_array().unwrap() {
            if member["name"].as_str() == Some("helper") {
                helper_members.push(member);
            }
        }
    }
    assert!(
        !helper_members.is_empty(),
        "expected `helper` member to carry a tagpath_handle after the name-collision fix: {json}"
    );
    for helper in &helper_members {
        assert_eq!(helper["tagpath_handle"].as_str(), Some(expected.as_str()));
        assert_eq!(helper["file"].as_str(), Some("src/main.rs"));
    }
    let ambiguity = json["community_diagnostics"]["ambiguous_members"]
        .as_array()
        .unwrap()
        .iter()
        .find(|diag| diag["name"].as_str() == Some("helper"))
        .unwrap_or_else(|| panic!("expected helper ambiguity diagnostic: {json}"));
    assert_eq!(ambiguity["candidate_count"].as_u64(), Some(2));
    assert_eq!(ambiguity["tagpath_candidate_count"].as_u64(), Some(1));
    assert_eq!(
        ambiguity["evidence"].as_str(),
        Some("unique_tagpath_handle")
    );
    assert_eq!(ambiguity["chosen_file"].as_str(), Some("src/main.rs"));
}

#[test]
fn communities_json_resolves_duplicate_member_handle_with_edge_context() {
    let dir = tempfile::tempdir().unwrap();
    fs::create_dir_all(dir.path().join("a_vendor")).unwrap();
    fs::write(dir.path().join("a_vendor/main.rs"), "fn helper() {}\n").unwrap();
    fs::create_dir_all(dir.path().join("src")).unwrap();
    fs::write(
        dir.path().join("src/main.rs"),
        "fn helper() {}\nfn src_caller() { helper(); }\nfn src_peer() { src_caller(); }\n",
    )
    .unwrap();

    let output = tsift_bin()
        .args(["index", dir.path().to_str().unwrap()])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "index stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    write_fresh_tagpath_index(
        dir.path(),
        &[
            ("helper", "a_vendor/main.rs"),
            ("helper", "src/main.rs"),
            ("src_caller", "src/main.rs"),
        ],
    );
    let expected = tagpath_member_handle(dir.path(), "helper", "src/main.rs");
    let vendor = tagpath_member_handle(dir.path(), "helper", "a_vendor/main.rs");
    assert_ne!(expected, vendor);

    let output = tsift_bin()
        .args(["communities", dir.path().to_str().unwrap(), "--json"])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "communities stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let json: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    let helper = json["communities"]
        .as_array()
        .unwrap()
        .iter()
        .flat_map(|community| community["members"].as_array().unwrap().iter())
        .find(|member| member["name"].as_str() == Some("helper"))
        .unwrap_or_else(|| panic!("expected helper community member: {json}"));

    assert_eq!(helper["tagpath_handle"].as_str(), Some(expected.as_str()));
    assert_ne!(helper["tagpath_handle"].as_str(), Some(vendor.as_str()));
    assert_eq!(helper["file"].as_str(), Some("src/main.rs"));
    assert!(
        helper["refs"]
            .as_array()
            .unwrap()
            .iter()
            .any(
                |reference| reference["file"].as_str() == Some("src/main.rs")
                    && reference["role"].as_str() == Some("callee")
                    && reference["peer"].as_str() == Some("src_caller")
            ),
        "expected helper refs to include src caller evidence: {helper}"
    );

    let ambiguity = json["community_diagnostics"]["ambiguous_members"]
        .as_array()
        .unwrap()
        .iter()
        .find(|diag| diag["name"].as_str() == Some("helper"))
        .unwrap_or_else(|| panic!("expected helper ambiguity diagnostic: {json}"));
    assert_eq!(ambiguity["candidate_count"].as_u64(), Some(2));
    assert_eq!(ambiguity["tagpath_candidate_count"].as_u64(), Some(2));
    assert_eq!(ambiguity["evidence"].as_str(), Some("edge_file"));
    assert_eq!(ambiguity["chosen_file"].as_str(), Some("src/main.rs"));
}

#[test]
fn communities_json_annotates_scoped_workspace_handles_from_per_scope_tagpath_indexes() {
    let dir = tempfile::tempdir().unwrap();
    fs::write(
        dir.path().join(".gitmodules"),
        r#"[submodule "src/alpha"]
	path = src/alpha
	url = https://example.com/alpha
[submodule "src/beta"]
	path = src/beta
	url = https://example.com/beta
"#,
    )
    .unwrap();

    for scope in ["alpha", "beta"] {
        let scope_root = dir.path().join(format!("src/{scope}"));
        fs::create_dir_all(&scope_root).unwrap();
        fs::write(
            scope_root.join("lib.rs"),
            format!("fn {scope}_helper() {{}}\nfn {scope}_caller() {{ {scope}_helper(); }}\n"),
        )
        .unwrap();
        let helper_name = format!("{scope}_helper");
        let caller_name = format!("{scope}_caller");
        write_fresh_tagpath_index(
            &scope_root,
            &[(&helper_name, "lib.rs"), (&caller_name, "lib.rs")],
        );
    }

    let output = tsift_bin()
        .args(["index", "--workspace", dir.path().to_str().unwrap()])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "workspace index stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    for scope in ["alpha", "beta"] {
        let scope_root = dir.path().join(format!("src/{scope}"));
        let helper_name = format!("{scope}_helper");
        let expected = tagpath_member_handle(&scope_root, &helper_name, "lib.rs");
        let output = tsift_bin()
            .args([
                "communities",
                dir.path().to_str().unwrap(),
                "--scope",
                scope,
                "--json",
            ])
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{scope} communities stderr: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        let json: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(
            json["community_diagnostics"]["tagpath_state"].as_str(),
            Some("fresh"),
            "{json}"
        );
        let helper = json["communities"]
            .as_array()
            .unwrap()
            .iter()
            .flat_map(|community| community["members"].as_array().unwrap().iter())
            .find(|member| member["name"].as_str() == Some(helper_name.as_str()))
            .unwrap_or_else(|| panic!("expected {helper_name} community member: {json}"));
        assert_eq!(helper["tagpath_handle"].as_str(), Some(expected.as_str()));
        assert_eq!(helper["file"].as_str(), Some("lib.rs"));
    }
}

#[test]
fn graph_json_resolves_callee_handle_with_caller_file_name_collision() {
    let dir = tempfile::tempdir().unwrap();
    fs::create_dir_all(dir.path().join("a_vendor")).unwrap();
    fs::write(
        dir.path().join("a_vendor/main.rs"),
        "fn helper() {}\nfn vendor_caller() { helper(); }\n",
    )
    .unwrap();
    fs::create_dir_all(dir.path().join("src")).unwrap();
    fs::write(
        dir.path().join("src/main.rs"),
        "fn helper() {}\nfn src_caller() { helper(); }\n",
    )
    .unwrap();

    let output = tsift_bin()
        .args(["index", dir.path().to_str().unwrap()])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "index stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    write_fresh_tagpath_index(
        dir.path(),
        &[
            ("helper", "a_vendor/main.rs"),
            ("helper", "src/main.rs"),
            ("src_caller", "src/main.rs"),
        ],
    );
    let expected = tagpath_member_handle(dir.path(), "helper", "src/main.rs");
    let vendor = tagpath_member_handle(dir.path(), "helper", "a_vendor/main.rs");
    assert_ne!(expected, vendor);

    let output = tsift_bin()
        .args([
            "graph",
            "src_caller",
            dir.path().to_str().unwrap(),
            "--json",
        ])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "graph stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let json: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    let helper_edge = json["callees"]
        .as_array()
        .unwrap()
        .iter()
        .find(|edge| edge["callee_name"].as_str() == Some("helper"))
        .unwrap_or_else(|| panic!("expected helper callee edge: {json}"));
    let actual = helper_edge["tagpath_handle"]
        .as_str()
        .unwrap_or_else(|| panic!("helper callee edge missing tagpath_handle: {helper_edge}"));
    assert_eq!(actual, expected);
    assert_ne!(actual, vendor);
}

#[test]
fn path_json_reports_shortest_symbol_chain() {
    let dir = indexed_cli_fixture();

    let output = tsift_bin()
        .args([
            "path",
            "main",
            "helper",
            dir.path().to_str().unwrap(),
            "--json",
        ])
        .output()
        .unwrap();

    assert!(output.status.success(), "path should succeed");
    let json: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();

    assert_eq!(json["from"], "main");
    assert_eq!(json["to"], "helper");
    assert_eq!(json["hops"].as_u64(), Some(3));
    let names: Vec<&str> = json["path"]
        .as_array()
        .unwrap()
        .iter()
        .map(|n| n["name"].as_str().unwrap())
        .collect();
    assert_eq!(names, vec!["main", "bridge", "shared", "helper"]);
    // Without a tagpath index in the fixture, no node should carry a handle.
    for node in json["path"].as_array().unwrap() {
        assert!(
            node.get("tagpath_handle").is_none(),
            "unexpected handle in {node}"
        );
    }
}

#[test]
fn path_json_omits_handles_when_no_tagpath_flag_set() {
    let dir = indexed_cli_fixture();
    write_fresh_tagpath_index(dir.path(), &[("main", "main.rs"), ("helper", "main.rs")]);

    let output = tsift_bin()
        .args([
            "path",
            "main",
            "helper",
            dir.path().to_str().unwrap(),
            "--json",
            "--no-tagpath",
        ])
        .output()
        .unwrap();

    assert!(output.status.success(), "path should succeed");
    let json: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    for node in json["path"].as_array().unwrap() {
        assert!(
            node.get("tagpath_handle").is_none(),
            "--no-tagpath should suppress handles: {node}"
        );
    }
}

#[test]
fn path_json_annotates_nodes_with_tagpath_handles_when_index_is_fresh() {
    let dir = indexed_cli_fixture();
    let members: Vec<(&str, &str)> = vec![
        ("main", "main.rs"),
        ("bridge", "main.rs"),
        ("shared", "main.rs"),
        ("helper", "main.rs"),
    ];
    write_fresh_tagpath_index(dir.path(), &members);

    let output = tsift_bin()
        .args([
            "path",
            "main",
            "helper",
            dir.path().to_str().unwrap(),
            "--json",
        ])
        .output()
        .unwrap();

    assert!(
        output.status.success(),
        "path should succeed (stderr={})",
        String::from_utf8_lossy(&output.stderr)
    );
    let json: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    let nodes = json["path"].as_array().unwrap();
    assert!(!nodes.is_empty());
    for node in nodes {
        let name = node["name"].as_str().unwrap();
        let handle = node
            .get("tagpath_handle")
            .and_then(|h| h.as_str())
            .unwrap_or_else(|| panic!("node {name} missing tagpath_handle"));
        assert!(
            handle.starts_with("mem:"),
            "expected mem: handle for {name}, got {handle}"
        );
    }
}

#[test]
fn explain_json_combines_definition_edges_and_community() {
    let dir = indexed_cli_fixture();

    let output = tsift_bin()
        .args(["explain", "alpha", dir.path().to_str().unwrap(), "--json"])
        .output()
        .unwrap();

    assert!(output.status.success(), "explain should succeed");
    let json: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();

    let definitions = json["definitions"].as_array().unwrap();
    assert!(
        definitions
            .iter()
            .any(|definition| definition["name"] == "alpha" && definition["file"] == "main.rs")
    );

    let callers = json["callers"].as_array().unwrap();
    assert!(callers.iter().any(|edge| edge["caller_name"] == "main"));
    assert!(callers.iter().any(|edge| edge["caller_name"] == "beta"));

    let callees = json["callees"].as_array().unwrap();
    assert!(callees.iter().any(|edge| edge["callee_name"] == "beta"));
    assert!(callees.iter().any(|edge| edge["callee_name"] == "gamma"));

    let community_members = json["community"]["members"].as_array().unwrap();
    let member_names: Vec<&str> = community_members
        .iter()
        .map(|m| m["name"].as_str().unwrap())
        .collect();
    assert!(member_names.contains(&"alpha"));
    assert!(member_names.contains(&"beta"));
    assert!(member_names.contains(&"gamma"));
    // Without a tagpath index in the fixture, no member should carry a handle.
    for member in community_members {
        assert!(
            member.get("tagpath_handle").is_none(),
            "unexpected handle in {member}"
        );
    }
    // Definitions/callers/callees stay handle-free without a tagpath index.
    for def in definitions {
        assert!(def.get("tagpath_handle").is_none(), "{def}");
    }
    for caller in callers {
        assert!(caller.get("tagpath_handle").is_none(), "{caller}");
    }
    for callee in callees {
        assert!(callee.get("tagpath_handle").is_none(), "{callee}");
    }
}

#[test]
fn explain_json_omits_handles_when_no_tagpath_flag_set() {
    let dir = indexed_cli_fixture();
    write_fresh_tagpath_index(dir.path(), &[("alpha", "main.rs"), ("beta", "main.rs")]);

    let output = tsift_bin()
        .args([
            "explain",
            "alpha",
            dir.path().to_str().unwrap(),
            "--json",
            "--no-tagpath",
        ])
        .output()
        .unwrap();

    assert!(output.status.success(), "explain should succeed");
    let json: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    for def in json["definitions"].as_array().unwrap() {
        assert!(
            def.get("tagpath_handle").is_none(),
            "--no-tagpath should suppress definition handles: {def}"
        );
    }
    for caller in json["callers"].as_array().unwrap() {
        assert!(
            caller.get("tagpath_handle").is_none(),
            "--no-tagpath should suppress caller handles: {caller}"
        );
    }
    for callee in json["callees"].as_array().unwrap() {
        assert!(
            callee.get("tagpath_handle").is_none(),
            "--no-tagpath should suppress callee handles: {callee}"
        );
    }
    for member in json["community"]["members"].as_array().unwrap() {
        assert!(
            member.get("tagpath_handle").is_none(),
            "--no-tagpath should suppress community handles: {member}"
        );
    }
}

#[test]
fn explain_json_annotates_definitions_edges_and_community_when_index_is_fresh() {
    let dir = indexed_cli_fixture();
    let members: Vec<(&str, &str)> = vec![
        ("alpha", "main.rs"),
        ("beta", "main.rs"),
        ("gamma", "main.rs"),
        ("main", "main.rs"),
    ];
    write_fresh_tagpath_index(dir.path(), &members);

    let output = tsift_bin()
        .args(["explain", "alpha", dir.path().to_str().unwrap(), "--json"])
        .output()
        .unwrap();

    assert!(
        output.status.success(),
        "explain should succeed (stderr={})",
        String::from_utf8_lossy(&output.stderr)
    );
    let json: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();

    let definitions = json["definitions"].as_array().unwrap();
    assert!(!definitions.is_empty());
    for def in definitions {
        let name = def["name"].as_str().unwrap();
        let handle = def["tagpath_handle"]
            .as_str()
            .unwrap_or_else(|| panic!("definition {name} missing tagpath_handle"));
        assert!(handle.starts_with("mem:"), "{name}: {handle}");
    }

    let callers = json["callers"].as_array().unwrap();
    assert!(!callers.is_empty());
    for caller in callers {
        let caller_name = caller["caller_name"].as_str().unwrap();
        let handle = caller["tagpath_handle"]
            .as_str()
            .unwrap_or_else(|| panic!("caller {caller_name} missing tagpath_handle"));
        assert!(handle.starts_with("mem:"), "{caller_name}: {handle}");
    }

    let callees = json["callees"].as_array().unwrap();
    assert!(!callees.is_empty());
    for callee in callees {
        let callee_name = callee["callee_name"].as_str().unwrap();
        let handle = callee["tagpath_handle"]
            .as_str()
            .unwrap_or_else(|| panic!("callee {callee_name} missing tagpath_handle"));
        assert!(handle.starts_with("mem:"), "{callee_name}: {handle}");
    }

    let community_members = json["community"]["members"].as_array().unwrap();
    assert!(!community_members.is_empty());
    for member in community_members {
        let name = member["name"].as_str().unwrap();
        let handle = member["tagpath_handle"]
            .as_str()
            .unwrap_or_else(|| panic!("community member {name} missing tagpath_handle"));
        assert!(handle.starts_with("mem:"), "{name}: {handle}");
    }
}

#[test]
fn source_read_json_reports_bounded_window_handles_and_expansion_commands() {
    let dir = indexed_cli_fixture();

    let output = tsift_bin()
        .args([
            "--envelope",
            "source-read",
            "main.rs",
            "--path",
            dir.path().to_str().unwrap(),
            "--style",
            "window",
            "--start",
            "1",
            "--lines",
            "8",
            "--json",
        ])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "source-read stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let json: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(json["tool"], "source-read");
    assert_eq!(json["view"], "window");
    assert!(
        json["report"]["handle"]
            .as_str()
            .unwrap()
            .starts_with("swin-")
    );
    assert_eq!(json["report"]["file"], "main.rs");
    assert_eq!(json["report"]["range"]["start"], 1);
    assert_eq!(json["report"]["range"]["end"], 8);
    assert!(
        json["report"]["range"]["truncated_after"]
            .as_bool()
            .unwrap()
    );
    assert_eq!(
        json["summary"]["metrics"]["_c"],
        serde_json::json!(["label", "value"])
    );
    assert_eq!(
        json["report"]["preview"]["_c"],
        serde_json::json!(["line", "text"])
    );
    let preview = structured_rows(&json["report"]["preview"]);
    assert_eq!(preview.len(), 8);
    assert!(preview[0]["text"].as_str().unwrap().contains("fn main"));
    assert!(
        json["report"]["expand"]["after"]
            .as_str()
            .unwrap()
            .contains("--start 9")
    );
    assert!(
        json["follow_up"]
            .as_array()
            .unwrap()
            .iter()
            .any(|cmd| cmd.as_str().unwrap().contains("source-read"))
    );

    let symbols = structured_rows(&json["report"]["symbols"]);
    let main_symbol = symbols
        .iter()
        .find(|symbol| symbol["name"] == "main")
        .unwrap_or_else(|| panic!("expected main symbol ref: {json}"));
    assert!(main_symbol["handle"].as_str().unwrap().starts_with("ssym-"));
    assert!(
        main_symbol["expand"]
            .as_str()
            .unwrap()
            .contains("tsift --envelope symbol-read")
    );
    assert!(
        main_symbol["span"]["handle"]
            .as_str()
            .unwrap()
            .starts_with("span-")
    );
    assert_eq!(main_symbol["span"]["node_kind"], "function_item");
    assert_eq!(main_symbol["span"]["start_byte"], 0);
    assert!(
        main_symbol["span"]["body_end_byte"].as_u64().unwrap()
            > main_symbol["span"]["body_start_byte"].as_u64().unwrap()
    );
}

#[test]
fn source_read_json_defaults_to_ast_symbol_projection() {
    let dir = indexed_cli_fixture();

    let output = tsift_bin()
        .args([
            "--envelope",
            "source-read",
            "main.rs",
            "--path",
            dir.path().to_str().unwrap(),
            "--json",
        ])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "source-read stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let json: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(json["tool"], "source-read");
    assert_eq!(json["view"], "ast");
    assert!(
        json["report"]["handle"]
            .as_str()
            .unwrap()
            .starts_with("sast-")
    );
    assert_eq!(json["report"]["file"], "main.rs");
    assert_eq!(json["report"]["range"]["start"], 1);
    assert_eq!(
        json["report"]["range"]["end"],
        json["report"]["range"]["total_lines"]
    );
    assert!(json["report"]["preview"].is_null());

    let symbols = structured_rows(&json["report"]["symbols"]);
    let main_symbol = symbols
        .iter()
        .find(|symbol| symbol["name"] == "main")
        .unwrap_or_else(|| panic!("expected main symbol ref: {json}"));
    assert!(
        main_symbol["expand"]
            .as_str()
            .unwrap()
            .contains("tsift --envelope symbol-read")
    );
    assert!(
        main_symbol["span"]["handle"]
            .as_str()
            .unwrap()
            .starts_with("span-")
    );
    assert!(
        json["report"]["expand"]["window"]
            .as_str()
            .unwrap()
            .contains("--style window")
    );
}

#[test]
fn source_read_json_reports_markdown_section_list_and_code_spans() {
    let dir = markdown_edit_fixture();

    let output = tsift_bin()
        .args([
            "--envelope",
            "source-read",
            "README.md",
            "--path",
            dir.path().to_str().unwrap(),
            "--style",
            "window",
            "--start",
            "1",
            "--lines",
            "24",
            "--json",
            "--budget",
            "normal",
        ])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "source-read markdown stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let json: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    let symbols = structured_rows(&json["report"]["symbols"]);
    let install = symbols
        .iter()
        .find(|symbol| symbol["kind"] == "heading" && symbol["name"] == "Install")
        .unwrap_or_else(|| panic!("expected Install heading ref: {json}"));
    assert_eq!(install["span"]["node_kind"], "atx_heading");
    assert_eq!(install["span"]["start_line"], 5);
    assert_eq!(install["span"]["end_line"], 17);
    assert_eq!(install["span"]["body_start_line"], 7);
    assert_eq!(install["span"]["markdown"]["heading_level"], 2);
    assert_eq!(
        install["span"]["markdown"]["section_path"],
        serde_json::json!(["Guide", "Install"])
    );
    assert!(
        install["span"]["parent_handle"]
            .as_str()
            .unwrap()
            .starts_with("span-")
    );
    assert!(
        install["span"]["child_handles"]
            .as_array()
            .unwrap()
            .iter()
            .any(|handle| handle.as_str().unwrap().starts_with("span-"))
    );

    let list_item = symbols
        .iter()
        .find(|symbol| symbol["kind"] == "list_item" && symbol["name"] == "Run setup.")
        .unwrap_or_else(|| panic!("expected Markdown list item ref: {json}"));
    assert_eq!(list_item["span"]["node_kind"], "list_item");
    assert_eq!(list_item["span"]["markdown"]["list_depth"], 0);
    assert_eq!(
        list_item["span"]["markdown"]["section_path"],
        serde_json::json!(["Guide", "Install"])
    );

    let code_block = symbols
        .iter()
        .find(|symbol| symbol["kind"] == "code_block" && symbol["name"] == "rust")
        .unwrap_or_else(|| panic!("expected Markdown code fence ref: {json}"));
    assert_eq!(code_block["span"]["node_kind"], "fenced_code_block");
    assert_eq!(code_block["span"]["markdown"]["fence_language"], "rust");
    assert_eq!(
        code_block["span"]["markdown"]["section_path"],
        serde_json::json!(["Guide", "Install"])
    );
    assert!(
        code_block["span"]["body_end_byte"].as_u64().unwrap()
            > code_block["span"]["body_start_byte"].as_u64().unwrap()
    );
    assert!(
        json["report"]["expand"]["markdown_ast"]
            .as_str()
            .unwrap()
            .contains("markdown-ast")
    );
    assert!(
        json["follow_up"]
            .as_array()
            .unwrap()
            .iter()
            .any(|cmd| cmd.as_str().unwrap().contains("markdown-ast"))
    );
    assert_eq!(json["report"]["markdown"]["mode"], "window_outline");
    assert!(
        json["report"]["markdown"]["visible_nodes"]
            .as_u64()
            .unwrap()
            >= 3,
        "expected visible Markdown AST nodes in source-read projection: {json}"
    );
    let outline = structured_rows(&json["report"]["markdown"]["outline"]);
    assert!(
        outline
            .iter()
            .any(|node| node["name"] == "Install" && node["kind"] == "heading"),
        "expected outline-first Markdown projection in source-read report: {json}"
    );
}

#[test]
fn markdown_ast_json_reports_handles_hierarchy_metadata_and_expansions() {
    let dir = markdown_edit_fixture();

    let output = tsift_bin()
        .args([
            "--envelope",
            "markdown-ast",
            "README.md",
            "--path",
            dir.path().to_str().unwrap(),
            "--json",
            "--budget",
            "deep",
        ])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "markdown-ast stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let json: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(json["tool"], "markdown-ast");
    assert_eq!(json["view"], "ast");
    assert_eq!(json["report"]["file"], "README.md");
    assert!(
        json["report"]["handle"]
            .as_str()
            .unwrap()
            .starts_with("mdastrep-")
    );
    assert_eq!(json["report"]["projection"]["mode"], "outline_first");
    assert!(
        json["report"]["projection"]["cache"]["source_hash"]
            .as_str()
            .unwrap()
            .len()
            >= 32
    );
    assert!(
        json["report"]["projection"]["phase_timings"]
            .as_array()
            .unwrap()
            .iter()
            .any(|phase| phase["name"] == "parse_extract")
    );
    assert!(
        json["report"]["projection"]["outline"]
            .as_array()
            .unwrap()
            .iter()
            .any(|node| node["kind"] == "heading" && node["name"] == "Guide")
    );
    assert!(
        json["report"]["expand"]["source_read"]
            .as_str()
            .unwrap()
            .contains("source-read")
    );
    assert!(
        json["report"]["expand"]["edit_intents"]
            .as_str()
            .unwrap()
            .contains("edit-intents")
    );

    let nodes = json["report"]["nodes"].as_array().unwrap();
    assert!(
        nodes.len() >= 7,
        "expected heading/list/code Markdown nodes, got {json}"
    );
    let guide = nodes
        .iter()
        .find(|node| node["kind"] == "heading" && node["name"] == "Guide")
        .unwrap_or_else(|| panic!("expected Guide heading node: {json}"));
    assert!(guide["handle"].as_str().unwrap().starts_with("mdast-"));
    assert!(guide["span_handle"].as_str().unwrap().starts_with("span-"));
    assert_eq!(guide["block_kind"], "section");
    assert_eq!(guide["metadata"]["heading_level"], 1);
    assert_eq!(
        guide["metadata"]["section_path"],
        serde_json::json!(["Guide"])
    );
    assert!(
        guide["child_handles"]
            .as_array()
            .unwrap()
            .iter()
            .any(|handle| handle.as_str().unwrap().starts_with("mdast-"))
    );

    let install = nodes
        .iter()
        .find(|node| node["kind"] == "heading" && node["name"] == "Install")
        .unwrap_or_else(|| panic!("expected Install heading node: {json}"));
    assert_eq!(
        install["metadata"]["section_path"],
        serde_json::json!(["Guide", "Install"])
    );
    assert_eq!(
        install["parent_handle"].as_str().unwrap(),
        guide["handle"].as_str().unwrap()
    );

    let list_item = nodes
        .iter()
        .find(|node| node["kind"] == "list_item" && node["name"] == "Run setup.")
        .unwrap_or_else(|| panic!("expected top-level list item node: {json}"));
    assert_eq!(list_item["metadata"]["list_depth"], 0);
    assert_eq!(list_item["metadata"]["list_marker"], "-");
    assert_eq!(
        list_item["metadata"]["section_path"],
        serde_json::json!(["Guide", "Install"])
    );

    let nested_item = nodes
        .iter()
        .find(|node| node["kind"] == "list_item" && node["name"] == "Confirm setup.")
        .unwrap_or_else(|| panic!("expected nested list item node: {json}"));
    assert_eq!(nested_item["metadata"]["list_depth"], 1);
    assert_eq!(
        nested_item["parent_handle"].as_str().unwrap(),
        list_item["handle"].as_str().unwrap()
    );

    let code_block = nodes
        .iter()
        .find(|node| node["kind"] == "code_block" && node["name"] == "rust")
        .unwrap_or_else(|| panic!("expected rust code fence node: {json}"));
    assert_eq!(code_block["block_kind"], "fenced_code_block");
    assert_eq!(code_block["metadata"]["fence_language"], "rust");
    assert_eq!(code_block["metadata"]["fence_marker"], "```");
    assert!(
        code_block["body_byte_span"]["end"].as_u64().unwrap()
            > code_block["body_byte_span"]["start"].as_u64().unwrap()
    );
    assert!(
        code_block["expand"]["source_window"]
            .as_str()
            .unwrap()
            .contains("source-read")
    );
    assert!(
        code_block["expand"]["symbol_read"]
            .as_str()
            .unwrap()
            .contains("symbol-read")
    );
    assert!(
        code_block["expand"]["edit_intents"]
            .as_str()
            .unwrap()
            .contains("edit-intents")
    );
    assert!(
        json["follow_up"]
            .as_array()
            .unwrap()
            .iter()
            .any(|cmd| cmd.as_str().unwrap().contains("source-read"))
    );
}

#[test]
fn markdown_ast_json_reports_selected_node_projection_mode() {
    let dir = markdown_edit_fixture();

    let all_output = tsift_bin()
        .args([
            "--envelope",
            "markdown-ast",
            "README.md",
            "--path",
            dir.path().to_str().unwrap(),
            "--json",
            "--budget",
            "deep",
        ])
        .output()
        .unwrap();
    assert!(
        all_output.status.success(),
        "markdown-ast all stderr: {}",
        String::from_utf8_lossy(&all_output.stderr)
    );
    let all_json: serde_json::Value = serde_json::from_slice(&all_output.stdout).unwrap();
    let install_handle = all_json["report"]["nodes"]
        .as_array()
        .unwrap()
        .iter()
        .find(|node| node["kind"] == "heading" && node["name"] == "Install")
        .and_then(|node| node["handle"].as_str())
        .unwrap();

    let output = tsift_bin()
        .args([
            "--envelope",
            "markdown-ast",
            "README.md",
            "--path",
            dir.path().to_str().unwrap(),
            "--node",
            install_handle,
            "--json",
            "--budget",
            "small",
        ])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "markdown-ast selected stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let json: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(json["report"]["projection"]["mode"], "selected_node");
    assert_eq!(
        json["report"]["projection"]["selected_node"],
        install_handle
    );
    let nodes = json["report"]["nodes"].as_array().unwrap();
    assert_eq!(nodes.len(), 1);
    assert_eq!(nodes[0]["handle"], install_handle);
    assert!(
        nodes[0]["expand"]["source_body"]
            .as_str()
            .unwrap()
            .contains("source-read")
    );
}

#[test]
fn symbol_read_markdown_reports_markdown_ast_span_expansion() {
    let dir = markdown_edit_fixture();

    let output = tsift_bin()
        .args([
            "--envelope",
            "symbol-read",
            "Install",
            "--file",
            "README.md",
            "--path",
            dir.path().to_str().unwrap(),
            "--json",
            "--budget",
            "normal",
        ])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "symbol-read markdown stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let json: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(json["tool"], "symbol-read");
    assert_eq!(json["report"]["symbol"]["language"], "markdown");
    let span_handle = json["report"]["symbol"]["span"]["handle"].as_str().unwrap();
    let markdown_ast = json["report"]["expand"]["markdown_ast"].as_str().unwrap();
    assert!(markdown_ast.contains("markdown-ast"), "{markdown_ast}");
    assert!(markdown_ast.contains("--node"), "{markdown_ast}");
    assert!(markdown_ast.contains(span_handle), "{markdown_ast}");
    assert!(
        json["follow_up"]
            .as_array()
            .unwrap()
            .iter()
            .any(|cmd| cmd.as_str().unwrap().contains("markdown-ast"))
    );
}

#[test]
fn symbol_read_json_reports_symbol_body_and_navigation_commands() {
    let dir = indexed_cli_fixture();

    let output = tsift_bin()
        .args([
            "--envelope",
            "symbol-read",
            "alpha",
            "--file",
            "main.rs",
            "--path",
            dir.path().to_str().unwrap(),
            "--json",
            "--budget",
            "normal",
        ])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "symbol-read stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let json: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(json["tool"], "symbol-read");
    assert_eq!(json["view"], "symbol");
    assert!(
        json["report"]["handle"]
            .as_str()
            .unwrap()
            .starts_with("sread-")
    );
    assert_eq!(json["report"]["symbol"]["name"], "alpha");
    assert_eq!(json["report"]["symbol"]["file"], "main.rs");
    assert!(
        json["report"]["symbol"]["span"]["handle"]
            .as_str()
            .unwrap()
            .starts_with("span-")
    );
    assert_eq!(
        json["report"]["symbol"]["span"]["node_kind"],
        "function_item"
    );
    assert_eq!(json["report"]["symbol"]["span"]["start_line"], 6);
    assert_eq!(json["report"]["symbol"]["span"]["end_line"], 9);
    assert!(
        json["report"]["symbol"]["span"]["body_end_byte"]
            .as_u64()
            .unwrap()
            > json["report"]["symbol"]["span"]["body_start_byte"]
                .as_u64()
                .unwrap()
    );
    assert!(
        json["report"]["body"]
            .as_array()
            .unwrap()
            .iter()
            .any(|line| line["text"].as_str().unwrap().contains("fn alpha"))
    );
    assert!(
        json["report"]["expand"]["explain"]
            .as_str()
            .unwrap()
            .contains("tsift --envelope explain")
    );
    assert!(
        json["report"]["expand"]["callers"]
            .as_str()
            .unwrap()
            .contains("--callers")
    );
    assert!(
        json["follow_up"]
            .as_array()
            .unwrap()
            .iter()
            .any(|cmd| cmd.as_str().unwrap().contains("source-read"))
    );
}

#[test]
fn edit_intents_json_validates_semantic_write_plan_without_mutating() {
    let dir = indexed_cli_fixture();
    let before = fs::read_to_string(dir.path().join("main.rs")).unwrap();
    let input = r#"{
        "intents": [
            {
                "kind": "rename_symbol",
                "symbol": "alpha",
                "file": "main.rs",
                "new_name": "alpha_renamed"
            },
            {
                "kind": "replace_function_body",
                "symbol": "beta",
                "file": "main.rs",
                "replacement": "gamma();"
            }
        ]
    }"#;

    let mut child = tsift_bin()
        .args([
            "--envelope",
            "edit-intents",
            "--path",
            dir.path().to_str().unwrap(),
            "--json",
            "--budget",
            "normal",
        ])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    {
        use std::io::Write;
        child
            .stdin
            .as_mut()
            .unwrap()
            .write_all(input.as_bytes())
            .unwrap();
    }
    let output = child.wait_with_output().unwrap();
    assert!(
        output.status.success(),
        "edit-intents stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        fs::read_to_string(dir.path().join("main.rs")).unwrap(),
        before
    );
    let json: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(json["tool"], "edit-intents");
    assert_eq!(json["view"], "dry-run");
    assert_eq!(json["report"]["mode"], "dry_run");
    assert_eq!(json["report"]["intents_total"], 2);
    assert_eq!(json["report"]["planned_total"], 2);
    assert_eq!(json["report"]["plans"].as_array().unwrap().len(), 2);
    assert!(
        json["report"]["plans"][0]["handle"]
            .as_str()
            .unwrap()
            .starts_with("eintent-")
    );
    assert_eq!(json["report"]["plans"][0]["target_symbol"]["name"], "alpha");
    assert!(
        json["report"]["plans"][0]["target_symbol"]["span"]["handle"]
            .as_str()
            .unwrap()
            .starts_with("span-")
    );
    assert_eq!(
        json["report"]["plans"][0]["target_symbol"]["span"]["node_kind"],
        "function_item"
    );
    assert_eq!(json["report"]["applied_total"], 0);
    assert_eq!(json["report"]["formatted_total"], 0);
    assert_eq!(json["report"]["plans"][0]["apply_supported"], true);
    assert_eq!(json["report"]["plans"][0]["applied"], false);
    assert!(
        json["report"]["plans"][0]["diff"]
            .as_str()
            .unwrap()
            .contains("alpha_renamed")
    );
    let patch = &json["report"]["plans"][0]["patch_proposal"];
    assert_eq!(patch["schema_version"], 1);
    assert_eq!(patch["strategy"], "ast_cst_minimal_textual_patch");
    assert_eq!(patch["status"], "ready");
    assert_eq!(patch["parser_state"]["input"], "valid");
    assert_eq!(patch["parser_state"]["output"], "valid");
    assert_eq!(patch["parser_state"]["validator"], "Rust");
    assert_eq!(patch["trivia"]["mode"], "preserve_unchanged_bytes");
    assert_eq!(patch["trivia"]["preserves_comments"], true);
    assert_eq!(patch["trivia"]["preserves_formatting"], true);
    assert_eq!(patch["trivia"]["preserves_trivia"], true);
    assert_eq!(patch["files"][0]["file"], "main.rs");
    assert_eq!(patch["files"][0]["language"], "rust");
    assert!(
        patch["files"][0]["hunks"][0]["diff"]
            .as_str()
            .unwrap()
            .contains("alpha_renamed")
    );
    assert!(
        patch["files"][0]["hunks"][0]["before"]["end_byte"]
            .as_u64()
            .unwrap()
            > patch["files"][0]["hunks"][0]["before"]["start_byte"]
                .as_u64()
                .unwrap()
    );
    assert!(
        json["follow_up"]
            .as_array()
            .unwrap()
            .iter()
            .any(|cmd| cmd.as_str().unwrap().contains("source-read"))
    );
}

#[test]
fn edit_intents_resolves_search_ast_span_handle_without_mutating() {
    let dir = indexed_cli_fixture();
    let before = fs::read_to_string(dir.path().join("main.rs")).unwrap();

    let search = tsift_bin()
        .args([
            "--envelope",
            "search",
            "alpha",
            "--path",
            dir.path().to_str().unwrap(),
            "--strategy",
            "lexical",
            "--json",
            "--budget",
            "normal",
        ])
        .output()
        .unwrap();
    assert!(
        search.status.success(),
        "search stderr: {}",
        String::from_utf8_lossy(&search.stderr)
    );
    let search_json: serde_json::Value = serde_json::from_slice(&search.stdout).unwrap();
    let alpha = search_json["report"]["symbols"]
        .as_array()
        .unwrap()
        .iter()
        .find(|symbol| symbol["name"] == "alpha")
        .unwrap_or_else(|| panic!("expected alpha search symbol: {search_json}"));
    let span_handle = alpha["ast"]["span"]["handle"].as_str().unwrap();

    let input = serde_json::json!({
        "intents": [{
            "kind": "rename_symbol",
            "target_handle": span_handle,
            "new_name": "alpha_from_handle"
        }]
    })
    .to_string();
    let output = run_tsift_stdin(
        &[
            "--envelope",
            "edit-intents",
            "--path",
            dir.path().to_str().unwrap(),
            "--json",
            "--budget",
            "normal",
        ],
        &input,
    );
    assert!(
        output.status.success(),
        "edit-intents stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        fs::read_to_string(dir.path().join("main.rs")).unwrap(),
        before
    );

    let json: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    let plan = &json["report"]["plans"][0];
    assert_eq!(plan["status"], "planned");
    assert_eq!(plan["applied"], false);
    assert_eq!(plan["target_symbol"]["name"], "alpha");
    assert_eq!(
        plan["target_symbol"]["span"]["start_byte"],
        alpha["ast"]["span"]["start_byte"]
    );
    assert_eq!(
        plan["target_selection"]["requested_handle"]
            .as_str()
            .unwrap(),
        span_handle
    );
    assert_eq!(
        plan["target_selection"]["matched_handle"].as_str().unwrap(),
        span_handle
    );
    assert_eq!(plan["target_selection"]["handle_family"], "ast_span");
    assert_eq!(
        plan["target_selection"]["source"],
        "search/source/symbol AST span"
    );
    assert!(
        plan["target_selection"]["source_window"]
            .as_str()
            .unwrap()
            .contains("source-read")
    );
}

#[test]
fn edit_intents_resolves_symbol_read_and_graph_handles_without_mutating() {
    let dir = indexed_cli_fixture();
    let before = fs::read_to_string(dir.path().join("main.rs")).unwrap();

    let symbol_read = tsift_bin()
        .args([
            "--envelope",
            "symbol-read",
            "alpha",
            "--path",
            dir.path().to_str().unwrap(),
            "--file",
            "main.rs",
            "--json",
            "--budget",
            "normal",
        ])
        .output()
        .unwrap();
    assert!(
        symbol_read.status.success(),
        "symbol-read stderr: {}",
        String::from_utf8_lossy(&symbol_read.stderr)
    );
    let symbol_json: serde_json::Value = serde_json::from_slice(&symbol_read.stdout).unwrap();
    let symbol_handle = symbol_json["report"]["symbol"]["handle"].as_str().unwrap();
    assert!(symbol_handle.starts_with("sread-"));

    let traverse = tsift_bin()
        .args([
            "traverse",
            "alpha",
            "--path",
            dir.path().to_str().unwrap(),
            "--format",
            "json",
        ])
        .output()
        .unwrap();
    assert!(
        traverse.status.success(),
        "traverse stderr: {}",
        String::from_utf8_lossy(&traverse.stderr)
    );
    let traverse_json: serde_json::Value = serde_json::from_slice(&traverse.stdout).unwrap();
    let graph_handle = traverse_json["nodes"]
        .as_array()
        .unwrap()
        .iter()
        .find(|node| node["kind"] == "symbol" && node["label"] == "alpha")
        .unwrap_or_else(|| panic!("expected alpha graph symbol: {traverse_json}"))["handle"]
        .as_str()
        .unwrap();
    assert!(graph_handle.starts_with("gsym-"));

    let input = serde_json::json!({
        "intents": [
            {
                "kind": "replace_function_body",
                "target_handle": symbol_handle,
                "replacement": "gamma();"
            },
            {
                "kind": "rename_symbol",
                "target_handle": graph_handle,
                "new_name": "alpha_graph"
            }
        ]
    })
    .to_string();
    let output = run_tsift_stdin(
        &[
            "--envelope",
            "edit-intents",
            "--path",
            dir.path().to_str().unwrap(),
            "--json",
            "--budget",
            "normal",
        ],
        &input,
    );
    assert!(
        output.status.success(),
        "edit-intents stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        fs::read_to_string(dir.path().join("main.rs")).unwrap(),
        before
    );

    let json: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(json["report"]["planned_total"], 2);
    assert_eq!(json["report"]["applied_total"], 0);
    let plans = json["report"]["plans"].as_array().unwrap();
    assert_eq!(plans[0]["target_selection"]["handle_family"], "symbol_read");
    assert_eq!(
        plans[0]["target_selection"]["requested_handle"]
            .as_str()
            .unwrap(),
        symbol_handle
    );
    assert_eq!(
        plans[0]["target_symbol"]["span"]["node_kind"],
        "function_item"
    );
    assert_eq!(
        plans[1]["target_selection"]["handle_family"],
        "graph_symbol"
    );
    assert_eq!(
        plans[1]["target_selection"]["requested_handle"]
            .as_str()
            .unwrap(),
        graph_handle
    );
    assert_eq!(plans[1]["target_symbol"]["name"], "alpha");
}

#[test]
fn edit_intents_patch_proposal_refuses_parse_errors_without_mutating() {
    let dir = indexed_cli_fixture();
    let path = dir.path().join("main.rs");
    let invalid = "fn alpha( {\n    beta();\n}\n";
    fs::write(&path, invalid).unwrap();

    let input = serde_json::json!({
        "intents": [{
            "kind": "rename_symbol",
            "symbol": "alpha",
            "file": "main.rs",
            "new_name": "alpha_from_invalid"
        }]
    })
    .to_string();
    let output = run_tsift_stdin(
        &[
            "--envelope",
            "edit-intents",
            "--path",
            dir.path().to_str().unwrap(),
            "--json",
            "--budget",
            "normal",
        ],
        &input,
    );
    assert!(
        output.status.success(),
        "edit-intents stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(fs::read_to_string(&path).unwrap(), invalid);

    let json: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(json["report"]["unsupported_total"], 1);
    let plan = &json["report"]["plans"][0];
    assert_eq!(plan["status"], "unsupported");
    assert_eq!(plan["apply_supported"], false);
    assert!(plan.get("patch_proposal").is_none());
    assert!(
        plan["message"]
            .as_str()
            .unwrap()
            .contains("patch proposal input produced Rust source with parse errors"),
        "plan message: {}",
        plan["message"]
    );
}

#[test]
fn edit_intents_apply_preserves_comments_formatting_and_generated_macro_sections() {
    let dir = ast_cst_rust_edit_fixture();
    let input = r#"{
        "intents": [
            {
                "kind": "insert_import",
                "file": "main.rs",
                "replacement": "std::fmt"
            },
            {
                "kind": "replace_function_body",
                "symbol": "alpha",
                "file": "main.rs",
                "replacement": "let value = make_value!() + 1;\nprintln!(\"value: {value}\");"
            }
        ]
    }"#;

    let output = run_tsift_stdin(
        &[
            "--envelope",
            "edit-intents",
            "--path",
            dir.path().to_str().unwrap(),
            "--apply",
            "--json",
            "--budget",
            "normal",
        ],
        input,
    );
    assert!(
        output.status.success(),
        "AST/CST apply stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let json: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(json["report"]["planned_total"], 2);
    assert_eq!(json["report"]["applied_total"], 2);
    assert_eq!(json["report"]["unsupported_total"], 0);

    let plans = json["report"]["plans"].as_array().unwrap();
    for plan in plans {
        let patch = &plan["patch_proposal"];
        assert_eq!(patch["strategy"], "ast_cst_minimal_textual_patch");
        assert_eq!(patch["parser_state"]["input"], "valid");
        assert_eq!(patch["parser_state"]["output"], "valid");
        assert_eq!(patch["trivia"]["preserves_comments"], true);
        assert_eq!(patch["trivia"]["preserves_formatting"], true);
        assert_eq!(patch["trivia"]["preserves_trivia"], true);
    }
    let body_hunk = &plans[1]["patch_proposal"]["files"][0]["hunks"][0];
    assert!(
        body_hunk["before"]["start_line"].as_u64().unwrap() > 12,
        "{}",
        body_hunk
    );

    let source = fs::read_to_string(dir.path().join("main.rs")).unwrap();
    assert!(source.contains("use std::fmt;"), "{source}");
    assert!(source.contains("use std::io;"), "{source}");
    assert!(
        source.contains("// Keep the module banner comment."),
        "{source}"
    );
    assert!(source.contains("// <generated:do-not-edit>"), "{source}");
    assert!(source.contains("macro_rules! make_value"), "{source}");
    assert!(source.contains("// </generated:do-not-edit>"), "{source}");
    assert!(source.contains("// Keep beta call comment."), "{source}");
    assert!(
        source.contains("let value = make_value!() + 1;"),
        "{source}"
    );
}

#[test]
fn edit_intents_markdown_rewrites_selected_fence_without_touching_mixed_language_blocks() {
    let dir = mixed_language_markdown_edit_fixture();
    let input = r#"{
        "intents": [
            {
                "kind": "rewrite_code_fence",
                "symbol": "rust",
                "file": "README.md",
                "replacement": "fn sample() {\n    println!(\"ok\");\n}\n"
            }
        ]
    }"#;

    let output = run_tsift_stdin(
        &[
            "--envelope",
            "edit-intents",
            "--path",
            dir.path().to_str().unwrap(),
            "--apply",
            "--json",
            "--budget",
            "normal",
        ],
        input,
    );
    assert!(
        output.status.success(),
        "mixed Markdown fence apply stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let json: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(json["report"]["planned_total"], 1);
    assert_eq!(json["report"]["applied_total"], 1);
    let plan = &json["report"]["plans"][0];
    assert_eq!(plan["target_symbol"]["kind"], "code_block");
    assert_eq!(
        plan["target_symbol"]["span"]["markdown"]["fence_language"],
        "rust"
    );
    assert_eq!(
        plan["patch_proposal"]["parser_state"]["validator"],
        "Markdown"
    );

    let readme = fs::read_to_string(dir.path().join("README.md")).unwrap();
    assert!(
        readme.contains("```rust\nfn sample() {\n    println!(\"ok\");\n}\n```"),
        "{readme}"
    );
    assert!(
        readme.contains("```ts\nfunction sample() {\n  return 1;\n}\n```"),
        "{readme}"
    );
    assert!(
        readme.contains("```python\ndef sample():\n    return 1\n```"),
        "{readme}"
    );
}

#[test]
fn edit_intents_apply_refuses_syntax_error_work_in_progress_source_without_mutating() {
    let dir = ast_cst_rust_edit_fixture();
    let path = dir.path().join("main.rs");
    let before = fs::read_to_string(&path).unwrap();
    let invalid = before.replace(
        "fn beta() {\n    alpha();\n}",
        "fn beta( {\n    alpha();\n}",
    );
    assert_ne!(before, invalid);
    fs::write(&path, &invalid).unwrap();

    let input = r#"{
        "intents": [
            {
                "kind": "replace_function_body",
                "symbol": "alpha",
                "file": "main.rs",
                "replacement": "let value = make_value!() + 1;"
            }
        ]
    }"#;

    let output = run_tsift_stdin(
        &[
            "--envelope",
            "edit-intents",
            "--path",
            dir.path().to_str().unwrap(),
            "--apply",
            "--json",
            "--budget",
            "normal",
        ],
        input,
    );
    assert!(
        !output.status.success(),
        "syntax-error WIP apply should fail"
    );
    assert_eq!(fs::read_to_string(&path).unwrap(), invalid);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("parse errors"), "stderr was: {stderr}");
}

#[test]
fn edit_intents_verify_failure_blocks_real_ast_cst_mutation() {
    let dir = git_ast_cst_rust_edit_fixture();
    let path = dir.path().join("main.rs");
    let before = fs::read_to_string(&path).unwrap();
    let input = r#"{
        "intents": [
            {
                "kind": "replace_function_body",
                "symbol": "alpha",
                "file": "main.rs",
                "replacement": "let value = make_value!() + 1;"
            }
        ]
    }"#;

    let output = run_tsift_stdin(
        &[
            "--envelope",
            "edit-intents",
            "--path",
            dir.path().to_str().unwrap(),
            "--json",
            "--verify",
            "--verify-command",
            "exit 23",
            "--apply",
            "--budget",
            "normal",
        ],
        input,
    );
    assert!(
        !output.status.success(),
        "failing AST/CST verify command should block apply"
    );
    assert_eq!(fs::read_to_string(&path).unwrap(), before);
    assert!(
        String::from_utf8_lossy(&output.stderr)
            .contains("semantic edit verification command failed"),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn edit_intents_markdown_contract_recognizes_heading_intent_without_mutating() {
    let dir = markdown_edit_fixture();
    let before = fs::read_to_string(dir.path().join("README.md")).unwrap();
    let input = r###"{
        "intents": [
            {
                "kind": "rename_heading",
                "symbol": "Guide",
                "file": "README.md",
                "new_name": "Manual"
            }
        ]
    }"###;

    let mut child = tsift_bin()
        .args([
            "--envelope",
            "edit-intents",
            "--path",
            dir.path().to_str().unwrap(),
            "--json",
            "--budget",
            "normal",
        ])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    {
        use std::io::Write;
        child
            .stdin
            .as_mut()
            .unwrap()
            .write_all(input.as_bytes())
            .unwrap();
    }
    let output = child.wait_with_output().unwrap();
    assert!(
        output.status.success(),
        "markdown edit-intents stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        fs::read_to_string(dir.path().join("README.md")).unwrap(),
        before
    );

    let json: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(json["tool"], "edit-intents");
    assert_eq!(json["view"], "dry-run");
    assert_eq!(json["report"]["mode"], "dry_run");
    assert_eq!(json["report"]["intents_total"], 1);
    assert_eq!(json["report"]["planned_total"], 1);
    assert_eq!(json["report"]["unsupported_total"], 0);

    let plan = &json["report"]["plans"][0];
    assert_eq!(plan["kind"], "rename_heading");
    assert_eq!(plan["status"], "planned");
    assert_eq!(plan["apply_supported"], true);
    assert_eq!(plan["applied"], false);
    assert_eq!(plan["target_file"], "README.md");
    assert_eq!(plan["target_symbol"]["name"], "Guide");
    assert_eq!(plan["target_symbol"]["kind"], "heading");
    assert_eq!(plan["target_symbol"]["language"], "markdown");
    assert_eq!(plan["target_symbol"]["line"], 1);
    assert_eq!(plan["target_symbol"]["end_line"], 20);
    assert!(
        plan["target_symbol"]["span"]["handle"]
            .as_str()
            .unwrap()
            .starts_with("span-")
    );
    assert_eq!(plan["target_symbol"]["span"]["node_kind"], "atx_heading");
    assert_eq!(
        plan["target_symbol"]["span"]["markdown"]["heading_level"],
        1
    );
    assert_eq!(plan["target_range"]["start"], 1);
    assert_eq!(plan["target_range"]["end"], 20);
    assert!(plan["diff"].as_str().unwrap().contains("# Manual"));
    assert!(
        plan["message"]
            .as_str()
            .unwrap()
            .contains("validated rename_heading")
    );
}

#[test]
fn edit_intents_apply_mutates_markdown_section_intents() {
    let dir = markdown_edit_fixture();
    let input = r###"{
        "intents": [
            {
                "kind": "rename_heading",
                "symbol": "Guide",
                "file": "README.md",
                "new_name": "Manual"
            },
            {
                "kind": "move_section",
                "symbol": "Troubleshooting",
                "file": "README.md",
                "destination_symbol": "Reference",
                "position": "after"
            },
            {
                "kind": "replace_section_body",
                "symbol": "Install",
                "file": "README.md",
                "replacement": "Install with cargo.\n"
            },
            {
                "kind": "insert_section",
                "symbol": "Reference",
                "file": "README.md",
                "position": "after",
                "replacement": "## Appendix\n\nExtra.\n"
            }
        ]
    }"###;

    let mut child = tsift_bin()
        .args([
            "--envelope",
            "edit-intents",
            "--path",
            dir.path().to_str().unwrap(),
            "--json",
            "--apply",
            "--budget",
            "normal",
        ])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    {
        use std::io::Write;
        child
            .stdin
            .as_mut()
            .unwrap()
            .write_all(input.as_bytes())
            .unwrap();
    }
    let output = child.wait_with_output().unwrap();
    assert!(
        output.status.success(),
        "markdown apply stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let json: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(json["tool"], "edit-intents");
    assert_eq!(json["report"]["mode"], "apply");
    assert_eq!(json["report"]["planned_total"], 4);
    assert_eq!(json["report"]["applied_total"], 4);
    assert_eq!(json["report"]["unsupported_total"], 0);
    assert_eq!(json["report"]["formatted_total"], 0);
    for plan in json["report"]["plans"].as_array().unwrap() {
        assert_eq!(plan["status"], "applied");
        assert_eq!(plan["apply_supported"], true);
        assert_eq!(plan["applied"], true);
    }

    let after = fs::read_to_string(dir.path().join("README.md")).unwrap();
    assert!(after.starts_with("# Manual\n\nIntro text."));
    assert!(after.contains("## Install\n\nInstall with cargo.\n\n## Reference"));
    assert!(after.contains("## Reference\n\nDone.\n\n### Troubleshooting\n\nCheck logs."));
    assert!(after.contains("## Appendix\n\nExtra."));
}

#[test]
fn edit_intents_apply_mutates_markdown_block_intents() {
    let dir = markdown_edit_fixture();
    let input = r#"{
        "intents": [
            {
                "kind": "insert_list_item",
                "symbol": "Confirm setup.",
                "file": "README.md",
                "position": "after",
                "replacement": "Verify setup."
            },
            {
                "kind": "rewrite_code_fence",
                "symbol": "rust",
                "file": "README.md",
                "replacement": "fn sample() {\n    println!(\"ok\");\n}\n"
            }
        ]
    }"#;

    let mut child = tsift_bin()
        .args([
            "--envelope",
            "edit-intents",
            "--path",
            dir.path().to_str().unwrap(),
            "--json",
            "--apply",
            "--budget",
            "normal",
        ])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    {
        use std::io::Write;
        child
            .stdin
            .as_mut()
            .unwrap()
            .write_all(input.as_bytes())
            .unwrap();
    }
    let output = child.wait_with_output().unwrap();
    assert!(
        output.status.success(),
        "markdown block apply stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let json: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(json["tool"], "edit-intents");
    assert_eq!(json["report"]["planned_total"], 2);
    assert_eq!(json["report"]["applied_total"], 2);
    assert_eq!(json["report"]["unsupported_total"], 0);
    for plan in json["report"]["plans"].as_array().unwrap() {
        assert_eq!(plan["status"], "applied");
        assert_eq!(plan["apply_supported"], true);
        assert_eq!(plan["applied"], true);
    }

    let after = fs::read_to_string(dir.path().join("README.md")).unwrap();
    assert!(
        after.contains("- Run setup.\n  - Confirm setup.\n  - Verify setup."),
        "{after}"
    );
    assert!(
        after.contains("```rust\nfn sample() {\n    println!(\"ok\");\n}\n```"),
        "{after}"
    );
}

#[test]
fn edit_intents_markdown_block_dry_run_reports_source_windows_without_mutating() {
    let dir = markdown_edit_fixture();
    let readme = dir.path().join("README.md");
    let before = fs::read_to_string(&readme).unwrap();
    let input = r#"{
        "intents": [
            {
                "kind": "insert_list_item",
                "symbol": "Confirm setup.",
                "file": "README.md",
                "position": "after",
                "replacement": "Verify setup."
            },
            {
                "kind": "rewrite_code_fence",
                "symbol": "rust",
                "file": "README.md",
                "replacement": "fn sample() {\n    println!(\"ok\");\n}\n"
            }
        ]
    }"#;

    let output = run_tsift_stdin(
        &[
            "--envelope",
            "edit-intents",
            "--path",
            dir.path().to_str().unwrap(),
            "--json",
            "--budget",
            "normal",
        ],
        input,
    );
    assert!(
        output.status.success(),
        "markdown block dry-run stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(fs::read_to_string(&readme).unwrap(), before);

    let json: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(json["tool"], "edit-intents");
    assert_eq!(json["view"], "dry-run");
    assert_eq!(json["report"]["mode"], "dry_run");
    assert_eq!(json["report"]["planned_total"], 2);
    assert_eq!(json["report"]["applied_total"], 0);
    assert_eq!(json["report"]["unsupported_total"], 0);

    let plans = json["report"]["plans"].as_array().unwrap();
    let list_plan = &plans[0];
    assert_eq!(list_plan["kind"], "insert_list_item");
    assert_eq!(list_plan["status"], "planned");
    assert_eq!(list_plan["apply_supported"], true);
    assert_eq!(list_plan["applied"], false);
    assert_eq!(list_plan["target_symbol"]["kind"], "list_item");
    assert_eq!(list_plan["target_symbol"]["language"], "markdown");
    assert_eq!(
        list_plan["target_symbol"]["span"]["markdown"]["list_depth"],
        1
    );
    assert!(
        list_plan["diff"]
            .as_str()
            .unwrap()
            .contains("Verify setup.")
    );

    let code_plan = &plans[1];
    assert_eq!(code_plan["kind"], "rewrite_code_fence");
    assert_eq!(code_plan["status"], "planned");
    assert_eq!(code_plan["apply_supported"], true);
    assert_eq!(code_plan["applied"], false);
    assert_eq!(code_plan["target_symbol"]["kind"], "code_block");
    assert_eq!(code_plan["target_symbol"]["language"], "markdown");
    assert_eq!(
        code_plan["target_symbol"]["span"]["markdown"]["fence_language"],
        "rust"
    );
    assert!(
        code_plan["diff"]
            .as_str()
            .unwrap()
            .contains("println!(\"ok\")")
    );
    assert!(json["follow_up"].as_array().unwrap().iter().any(|cmd| {
        let cmd = cmd.as_str().unwrap();
        cmd.contains("source-read") && cmd.contains("README.md")
    }));
}

#[test]
fn edit_intents_markdown_block_apply_refuses_ambiguous_targets_without_mutating() {
    let dir = tempfile::tempdir().unwrap();
    fs::write(
        dir.path().join("README.md"),
        "# Guide\n\n- Repeat\n- Repeat\n\n```rust\none();\n```\n\n```rust\ntwo();\n```\n",
    )
    .unwrap();
    let output = tsift_bin()
        .args(["index", dir.path().to_str().unwrap()])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "index stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let before = fs::read_to_string(dir.path().join("README.md")).unwrap();
    let input = r#"{
        "intents": [
            {
                "kind": "insert_list_item",
                "symbol": "Repeat",
                "file": "README.md",
                "replacement": "Added"
            },
            {
                "kind": "rewrite_code_fence",
                "symbol": "rust",
                "file": "README.md",
                "replacement": "updated();\n"
            }
        ]
    }"#;

    let mut child = tsift_bin()
        .args([
            "--envelope",
            "edit-intents",
            "--path",
            dir.path().to_str().unwrap(),
            "--apply",
            "--json",
        ])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    {
        use std::io::Write;
        child
            .stdin
            .as_mut()
            .unwrap()
            .write_all(input.as_bytes())
            .unwrap();
    }
    let output = child.wait_with_output().unwrap();
    assert!(
        !output.status.success(),
        "ambiguous markdown apply should fail"
    );
    assert_eq!(
        fs::read_to_string(dir.path().join("README.md")).unwrap(),
        before
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("unsupported"), "stderr was: {stderr}");
    assert!(stderr.contains("ambiguous"), "stderr was: {stderr}");
}

#[test]
fn edit_intents_markdown_block_apply_refuses_fence_marker_replacement_without_mutating() {
    let dir = markdown_edit_fixture();
    let readme = dir.path().join("README.md");
    let before = fs::read_to_string(&readme).unwrap();
    let input = r#"{
        "intents": [
            {
                "kind": "rewrite_code_fence",
                "symbol": "rust",
                "file": "README.md",
                "replacement": "```rust\nfn sample() {}\n```\n"
            }
        ]
    }"#;

    let output = run_tsift_stdin(
        &[
            "--envelope",
            "edit-intents",
            "--path",
            dir.path().to_str().unwrap(),
            "--apply",
            "--json",
        ],
        input,
    );
    assert!(
        !output.status.success(),
        "fence-marker replacement should fail"
    );
    assert_eq!(fs::read_to_string(&readme).unwrap(), before);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("unsupported"), "stderr was: {stderr}");
    assert!(stderr.contains("fence markers"), "stderr was: {stderr}");
}

#[test]
fn edit_intents_markdown_verify_reports_temp_worktree_source_read_and_impact() {
    let dir = git_markdown_edit_fixture();
    let readme = dir.path().join("README.md");
    let before = fs::read_to_string(&readme).unwrap();
    let input = r#"{
        "intents": [
            {
                "kind": "insert_list_item",
                "symbol": "Confirm setup.",
                "file": "README.md",
                "position": "after",
                "replacement": "Verify setup."
            },
            {
                "kind": "rewrite_code_fence",
                "symbol": "rust",
                "file": "README.md",
                "replacement": "fn sample() {\n    println!(\"ok\");\n}\n"
            }
        ]
    }"#;

    let output = run_tsift_stdin(
        &[
            "--envelope",
            "edit-intents",
            "--path",
            dir.path().to_str().unwrap(),
            "--json",
            "--verify",
            "--budget",
            "normal",
        ],
        input,
    );
    assert!(
        output.status.success(),
        "markdown verify stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(fs::read_to_string(&readme).unwrap(), before);

    let json: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(json["tool"], "edit-intents");
    assert_eq!(json["view"], "verify");
    assert_eq!(json["report"]["mode"], "verify");
    assert_eq!(json["report"]["planned_total"], 2);
    assert_eq!(json["report"]["applied_total"], 0);
    assert_eq!(json["report"]["unsupported_total"], 0);

    let verification = &json["report"]["verification"];
    assert_eq!(verification["status"], "passed");
    assert_eq!(verification["worktree"], "temporary git worktree at HEAD");
    assert_eq!(verification["reindexed"], true);
    assert_eq!(verification["temp_applied_total"], 2);
    assert_eq!(verification["temp_formatted_total"], 0);
    assert!(
        verification["message"]
            .as_str()
            .unwrap()
            .contains("temporary worktree")
    );
    assert!(
        verification["source_reads"]
            .as_array()
            .unwrap()
            .iter()
            .any(|read| {
                read["file"] == "README.md"
                    && read["preview_lines"].as_u64().unwrap() > 0
                    && read["command"].as_str().unwrap().contains("source-read")
                    && read["command"].as_str().unwrap().contains("README.md")
            })
    );
    assert!(verification["impact"]["changed_files"].as_u64().unwrap() >= 1);
}

#[test]
fn edit_intents_markdown_verify_command_failure_blocks_real_mutation() {
    let dir = git_markdown_edit_fixture();
    let readme = dir.path().join("README.md");
    let before = fs::read_to_string(&readme).unwrap();
    let input = r#"{
        "intents": [
            {
                "kind": "insert_list_item",
                "symbol": "Confirm setup.",
                "file": "README.md",
                "position": "after",
                "replacement": "Verify setup."
            }
        ]
    }"#;

    let output = run_tsift_stdin(
        &[
            "--envelope",
            "edit-intents",
            "--path",
            dir.path().to_str().unwrap(),
            "--json",
            "--verify",
            "--verify-command",
            "exit 7",
            "--apply",
            "--budget",
            "normal",
        ],
        input,
    );
    assert!(
        !output.status.success(),
        "failing Markdown verify command should block apply"
    );
    assert_eq!(fs::read_to_string(&readme).unwrap(), before);
    assert!(
        String::from_utf8_lossy(&output.stderr)
            .contains("semantic edit verification command failed"),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn edit_intents_verify_uses_temp_worktree_without_mutating_source() {
    let dir = git_indexed_cli_fixture();
    let before = fs::read_to_string(dir.path().join("main.rs")).unwrap();
    let input = r#"{
        "intents": [
            {
                "kind": "rename_symbol",
                "symbol": "alpha",
                "file": "main.rs",
                "new_name": "alpha_verified"
            }
        ]
    }"#;

    let mut child = tsift_bin()
        .args([
            "--envelope",
            "edit-intents",
            "--path",
            dir.path().to_str().unwrap(),
            "--json",
            "--verify",
            "--budget",
            "normal",
        ])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    {
        use std::io::Write;
        child
            .stdin
            .as_mut()
            .unwrap()
            .write_all(input.as_bytes())
            .unwrap();
    }
    let output = child.wait_with_output().unwrap();
    assert!(
        output.status.success(),
        "edit-intents --verify stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        fs::read_to_string(dir.path().join("main.rs")).unwrap(),
        before
    );

    let json: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(json["tool"], "edit-intents");
    assert_eq!(json["view"], "verify");
    assert_eq!(json["report"]["mode"], "verify");
    assert_eq!(json["report"]["applied_total"], 0);
    assert_eq!(json["report"]["verification"]["status"], "passed");
    assert_eq!(json["report"]["verification"]["temp_applied_total"], 1);
    assert_eq!(json["report"]["verification"]["reindexed"], true);
    assert!(
        json["report"]["verification"]["source_reads"]
            .as_array()
            .unwrap()
            .iter()
            .any(|read| read["file"] == "main.rs" && read["preview_lines"].as_u64().unwrap() > 0)
    );
    assert!(
        json["report"]["verification"]["impact"]["changed_files"]
            .as_u64()
            .unwrap()
            >= 1
    );
}

#[test]
fn edit_intents_verify_apply_runs_command_before_real_mutation() {
    let dir = git_indexed_cli_fixture();
    let input = r#"{
        "intents": [
            {
                "kind": "rename_symbol",
                "symbol": "alpha",
                "file": "main.rs",
                "new_name": "alpha_verified"
            }
        ]
    }"#;

    let mut child = tsift_bin()
        .args([
            "--envelope",
            "edit-intents",
            "--path",
            dir.path().to_str().unwrap(),
            "--json",
            "--verify",
            "--verify-command",
            "test -f main.rs",
            "--apply",
            "--budget",
            "normal",
        ])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    {
        use std::io::Write;
        child
            .stdin
            .as_mut()
            .unwrap()
            .write_all(input.as_bytes())
            .unwrap();
    }
    let output = child.wait_with_output().unwrap();
    assert!(
        output.status.success(),
        "edit-intents --verify --apply stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let json: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(json["tool"], "edit-intents");
    assert_eq!(json["view"], "apply");
    assert_eq!(json["report"]["applied_total"], 1);
    assert_eq!(json["report"]["verification"]["status"], "passed");
    assert_eq!(
        json["report"]["verification"]["command"]["status"],
        "passed"
    );
    let source = fs::read_to_string(dir.path().join("main.rs")).unwrap();
    assert!(source.contains("fn alpha_verified()"), "{source}");
}

#[test]
fn edit_intents_verify_command_failure_blocks_real_mutation() {
    let dir = git_indexed_cli_fixture();
    let before = fs::read_to_string(dir.path().join("main.rs")).unwrap();
    let input = r#"{
        "intents": [
            {
                "kind": "rename_symbol",
                "symbol": "alpha",
                "file": "main.rs",
                "new_name": "alpha_verified"
            }
        ]
    }"#;

    let mut child = tsift_bin()
        .args([
            "--envelope",
            "edit-intents",
            "--path",
            dir.path().to_str().unwrap(),
            "--json",
            "--verify",
            "--verify-command",
            "exit 7",
            "--apply",
            "--budget",
            "normal",
        ])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    {
        use std::io::Write;
        child
            .stdin
            .as_mut()
            .unwrap()
            .write_all(input.as_bytes())
            .unwrap();
    }
    let output = child.wait_with_output().unwrap();
    assert!(!output.status.success());
    assert!(
        String::from_utf8_lossy(&output.stderr)
            .contains("semantic edit verification command failed"),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        fs::read_to_string(dir.path().join("main.rs")).unwrap(),
        before
    );
}

#[test]
fn edit_intents_apply_formats_and_mutates_supported_rust_intents() {
    let dir = indexed_cli_fixture();
    let input = r#"{
        "intents": [
            {
                "kind": "insert_import",
                "file": "main.rs",
                "replacement": "std::fmt"
            },
            {
                "kind": "rename_symbol",
                "symbol": "alpha",
                "file": "main.rs",
                "new_name": "alpha_renamed"
            },
            {
                "kind": "replace_function_body",
                "symbol": "beta",
                "file": "main.rs",
                "replacement": "gamma();"
            }
        ]
    }"#;

    let mut child = tsift_bin()
        .args([
            "--envelope",
            "edit-intents",
            "--path",
            dir.path().to_str().unwrap(),
            "--json",
            "--apply",
            "--budget",
            "normal",
        ])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    {
        use std::io::Write;
        child
            .stdin
            .as_mut()
            .unwrap()
            .write_all(input.as_bytes())
            .unwrap();
    }
    let output = child.wait_with_output().unwrap();
    assert!(
        output.status.success(),
        "edit-intents --apply stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let json: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(json["tool"], "edit-intents");
    assert_eq!(json["view"], "apply");
    assert_eq!(json["report"]["mode"], "apply");
    assert_eq!(json["report"]["planned_total"], 3);
    assert_eq!(json["report"]["applied_total"], 3);
    assert_eq!(json["report"]["formatted_total"], 1);
    assert!(
        json["report"]["plans"]
            .as_array()
            .unwrap()
            .iter()
            .all(|plan| {
                plan["status"] == "applied"
                    && plan["applied"] == true
                    && plan["formatter"] == "rustfmt --edition 2024"
            })
    );

    let source = fs::read_to_string(dir.path().join("main.rs")).unwrap();
    assert!(source.contains("use std::fmt;"), "{source}");
    assert!(source.contains("fn alpha_renamed()"), "{source}");
    assert!(source.contains("alpha_renamed();"), "{source}");
    assert!(source.contains("fn beta() {\n    gamma();\n}"), "{source}");
}

#[test]
fn edit_intents_insert_import_preserves_rust_inner_prelude() {
    let dir = tempfile::tempdir().unwrap();
    fs::write(
        dir.path().join("main.rs"),
        r#"//! crate docs
#![allow(dead_code)]

use std::io;

fn main() {}
"#,
    )
    .unwrap();
    let index = tsift_bin()
        .args(["index", dir.path().to_str().unwrap()])
        .output()
        .unwrap();
    assert!(
        index.status.success(),
        "index stderr: {}",
        String::from_utf8_lossy(&index.stderr)
    );

    let input = r#"{
        "intents": [
            {
                "kind": "insert_import",
                "file": "main.rs",
                "replacement": "std::fmt"
            }
        ]
    }"#;
    let output = run_tsift_stdin(
        &[
            "--envelope",
            "edit-intents",
            "--path",
            dir.path().to_str().unwrap(),
            "--apply",
            "--json",
        ],
        input,
    );
    assert!(
        output.status.success(),
        "insert_import stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let json: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(json["report"]["planned_total"], 1);
    assert_eq!(json["report"]["applied_total"], 1);
    let source = fs::read_to_string(dir.path().join("main.rs")).unwrap();
    assert!(
        source.starts_with("//! crate docs\n#![allow(dead_code)]"),
        "{source}"
    );
    let attr_pos = source.find("#![allow(dead_code)]").unwrap();
    let import_pos = source.find("use std::fmt;").unwrap();
    let fn_pos = source.find("fn main").unwrap();
    assert!(attr_pos < import_pos, "{source}");
    assert!(import_pos < fn_pos, "{source}");
}

#[test]
fn edit_intents_replace_function_body_uses_rust_ast_body_range() {
    let dir = tempfile::tempdir().unwrap();
    fs::write(
        dir.path().join("main.rs"),
        r#"const TEMPLATE: &str = "fn beta() { ignored(); }";

fn alpha() {}
fn gamma() {}

fn beta() {
    alpha();
}
"#,
    )
    .unwrap();
    let index = tsift_bin()
        .args(["index", dir.path().to_str().unwrap()])
        .output()
        .unwrap();
    assert!(
        index.status.success(),
        "index stderr: {}",
        String::from_utf8_lossy(&index.stderr)
    );

    let search = tsift_bin()
        .args([
            "--envelope",
            "search",
            "beta",
            "--path",
            dir.path().to_str().unwrap(),
            "--strategy",
            "lexical",
            "--json",
            "--budget",
            "normal",
        ])
        .output()
        .unwrap();
    assert!(
        search.status.success(),
        "search stderr: {}",
        String::from_utf8_lossy(&search.stderr)
    );
    let search_json: serde_json::Value = serde_json::from_slice(&search.stdout).unwrap();
    let beta = search_json["report"]["symbols"]
        .as_array()
        .unwrap()
        .iter()
        .find(|symbol| symbol["name"] == "beta")
        .unwrap_or_else(|| panic!("expected beta search symbol: {search_json}"));
    let target_handle = beta["ast"]["span"]["handle"].as_str().unwrap();

    let input = serde_json::json!({
        "intents": [{
            "kind": "replace_function_body",
            "target_handle": target_handle,
            "replacement": "gamma();"
        }]
    })
    .to_string();
    let output = run_tsift_stdin(
        &[
            "--envelope",
            "edit-intents",
            "--path",
            dir.path().to_str().unwrap(),
            "--apply",
            "--json",
        ],
        &input,
    );
    assert!(
        output.status.success(),
        "replace_function_body stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let json: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    let plan = &json["report"]["plans"][0];
    assert_eq!(plan["status"], "applied");
    assert_eq!(plan["target_selection"]["requested_handle"], target_handle);
    let source = fs::read_to_string(dir.path().join("main.rs")).unwrap();
    assert!(
        source.contains("const TEMPLATE: &str = \"fn beta() { ignored(); }\";"),
        "{source}"
    );
    assert!(source.contains("fn beta() {\n    gamma();\n}"), "{source}");
    assert!(!source.contains("fn beta() {\n    alpha();\n}"), "{source}");
}

#[test]
fn edit_intents_apply_rewrites_indexed_rust_call_sites() {
    let dir = indexed_cli_fixture();
    let input = r#"{
        "intents": [
            {
                "kind": "rewrite_call_sites",
                "symbol": "gamma",
                "file": "main.rs",
                "replacement": "gamma_twice()"
            }
        ]
    }"#;

    let mut child = tsift_bin()
        .args([
            "--envelope",
            "edit-intents",
            "--path",
            dir.path().to_str().unwrap(),
            "--apply",
            "--json",
        ])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    {
        use std::io::Write;
        child
            .stdin
            .as_mut()
            .unwrap()
            .write_all(input.as_bytes())
            .unwrap();
    }
    let output = child.wait_with_output().unwrap();
    assert!(
        output.status.success(),
        "rewrite_call_sites stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let json: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    let plan = &json["report"]["plans"][0];
    assert_eq!(plan["kind"], "rewrite_call_sites");
    assert_eq!(plan["status"], "applied");
    assert_eq!(plan["applied"], true);
    assert_eq!(plan["apply_supported"], true);
    let call_lines = plan["call_refs"]
        .as_array()
        .unwrap()
        .iter()
        .map(|call| call["line"].as_u64().unwrap())
        .collect::<Vec<_>>();
    assert_eq!(call_lines, vec![8, 13]);

    let source = fs::read_to_string(dir.path().join("main.rs")).unwrap();
    assert!(source.contains("fn gamma()"), "{source}");
    assert!(
        source.contains("fn alpha() {\n    beta();\n    gamma_twice();\n}"),
        "{source}"
    );
    assert!(
        source.contains("fn beta() {\n    alpha();\n    gamma_twice();\n}"),
        "{source}"
    );
}

#[test]
fn edit_intents_apply_updates_rust_signature_and_call_sites() {
    let dir = indexed_cli_fixture();
    let input = r#"{
        "intents": [
            {
                "kind": "update_call_signature",
                "symbol": "beta",
                "file": "main.rs",
                "replacement": "fn beta(value: i32)",
                "call_replacement": "beta(7)"
            }
        ]
    }"#;

    let mut child = tsift_bin()
        .args([
            "--envelope",
            "edit-intents",
            "--path",
            dir.path().to_str().unwrap(),
            "--apply",
            "--json",
        ])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    {
        use std::io::Write;
        child
            .stdin
            .as_mut()
            .unwrap()
            .write_all(input.as_bytes())
            .unwrap();
    }
    let output = child.wait_with_output().unwrap();
    assert!(
        output.status.success(),
        "update_call_signature stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let json: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    let plan = &json["report"]["plans"][0];
    assert_eq!(plan["kind"], "update_call_signature");
    assert_eq!(plan["status"], "applied");
    let call_lines = plan["call_refs"]
        .as_array()
        .unwrap()
        .iter()
        .map(|call| call["line"].as_u64().unwrap())
        .collect::<Vec<_>>();
    assert_eq!(call_lines, vec![7, 18]);

    let source = fs::read_to_string(dir.path().join("main.rs")).unwrap();
    assert!(source.contains("fn beta(value: i32)"), "{source}");
    assert!(
        source.contains("fn alpha() {\n    beta(7);\n    gamma();\n}"),
        "{source}"
    );
    assert!(
        source.contains("fn gamma() {\n    alpha();\n    beta(7);\n}"),
        "{source}"
    );
}

#[test]
fn edit_intents_apply_refuses_signature_update_without_call_replacement() {
    let dir = indexed_cli_fixture();
    let before = fs::read_to_string(dir.path().join("main.rs")).unwrap();
    let input = r#"{
        "intents": [
            {
                "kind": "update_call_signature",
                "symbol": "beta",
                "file": "main.rs",
                "replacement": "fn beta(value: i32)"
            }
        ]
    }"#;

    let mut child = tsift_bin()
        .args([
            "edit-intents",
            "--path",
            dir.path().to_str().unwrap(),
            "--apply",
            "--json",
        ])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    {
        use std::io::Write;
        child
            .stdin
            .as_mut()
            .unwrap()
            .write_all(input.as_bytes())
            .unwrap();
    }
    let output = child.wait_with_output().unwrap();
    assert!(
        !output.status.success(),
        "missing call_replacement should fail"
    );
    assert_eq!(
        fs::read_to_string(dir.path().join("main.rs")).unwrap(),
        before
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("unsupported"), "stderr was: {stderr}");
}

#[test]
fn edit_intents_apply_adds_rust_method_to_struct_impl() {
    let dir = structural_edit_fixture();
    let input = r#"{
        "intents": [
            {
                "kind": "add_method",
                "symbol": "Widget",
                "replacement": "pub fn value(&self) -> i32 { self.value }"
            }
        ]
    }"#;

    let mut child = tsift_bin()
        .args([
            "--envelope",
            "edit-intents",
            "--path",
            dir.path().to_str().unwrap(),
            "--apply",
            "--json",
        ])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    {
        use std::io::Write;
        child
            .stdin
            .as_mut()
            .unwrap()
            .write_all(input.as_bytes())
            .unwrap();
    }
    let output = child.wait_with_output().unwrap();
    assert!(
        output.status.success(),
        "add_method stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let json: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    let plan = &json["report"]["plans"][0];
    assert_eq!(plan["kind"], "add_method");
    assert_eq!(plan["status"], "applied");
    assert_eq!(plan["target_symbol"]["kind"], "struct");
    assert_eq!(plan["formatter"], "rustfmt --edition 2024");

    let source = fs::read_to_string(dir.path().join("main.rs")).unwrap();
    assert!(
        source.contains(
            "impl Widget {\n    pub fn value(&self) -> i32 {\n        self.value\n    }\n}"
        ),
        "{source}"
    );
}

#[test]
fn edit_intents_apply_moves_rust_declaration_between_module_files() {
    let dir = structural_edit_fixture();
    let input = r#"{
        "intents": [
            {
                "kind": "move_declaration",
                "symbol": "moved",
                "file": "widget.rs"
            }
        ]
    }"#;

    let mut child = tsift_bin()
        .args([
            "--envelope",
            "edit-intents",
            "--path",
            dir.path().to_str().unwrap(),
            "--apply",
            "--json",
        ])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    {
        use std::io::Write;
        child
            .stdin
            .as_mut()
            .unwrap()
            .write_all(input.as_bytes())
            .unwrap();
    }
    let output = child.wait_with_output().unwrap();
    assert!(
        output.status.success(),
        "move_declaration stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let json: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    let plan = &json["report"]["plans"][0];
    assert_eq!(plan["kind"], "move_declaration");
    assert_eq!(plan["status"], "applied");
    assert_eq!(plan["target_file"], "main.rs");
    assert_eq!(plan["destination_file"], "widget.rs");

    let source = fs::read_to_string(dir.path().join("main.rs")).unwrap();
    assert!(source.contains("mod widget;"), "{source}");
    assert!(source.contains("use widget::moved;"), "{source}");
    assert!(!source.contains("pub fn moved()"), "{source}");
    assert!(
        source.contains("fn caller() -> i32 {\n    moved()\n}"),
        "{source}"
    );

    let destination = fs::read_to_string(dir.path().join("widget.rs")).unwrap();
    assert!(
        destination.contains("pub fn moved() -> i32 {\n    7\n}"),
        "{destination}"
    );
    assert!(
        destination.contains("pub fn existing() -> i32 {\n    1\n}"),
        "{destination}"
    );
}

#[test]
fn edit_intents_apply_refuses_move_declaration_without_mutating() {
    let dir = structural_edit_fixture();
    let before_source = fs::read_to_string(dir.path().join("main.rs")).unwrap();
    let before_destination = fs::read_to_string(dir.path().join("widget.rs")).unwrap();
    let input = r#"{
        "intents": [
            {
                "kind": "move_declaration",
                "symbol": "moved",
                "file": "main.rs"
            }
        ]
    }"#;

    let mut child = tsift_bin()
        .args([
            "edit-intents",
            "--path",
            dir.path().to_str().unwrap(),
            "--apply",
            "--json",
        ])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    {
        use std::io::Write;
        child
            .stdin
            .as_mut()
            .unwrap()
            .write_all(input.as_bytes())
            .unwrap();
    }
    let output = child.wait_with_output().unwrap();
    assert!(
        !output.status.success(),
        "same-file move_declaration should fail"
    );
    assert_eq!(
        fs::read_to_string(dir.path().join("main.rs")).unwrap(),
        before_source
    );
    assert_eq!(
        fs::read_to_string(dir.path().join("widget.rs")).unwrap(),
        before_destination
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("unsupported"), "stderr was: {stderr}");
}

#[test]
fn edit_intents_apply_mutates_typescript_executor_intents() {
    let dir = script_edit_fixture();
    let input = r#"{
        "intents": [
            {
                "kind": "insert_import",
                "file": "tool.ts",
                "replacement": "{ extra } from \"./extra\""
            },
            {
                "kind": "replace_function_body",
                "symbol": "alpha",
                "file": "tool.ts",
                "replacement": "return value * 2;"
            }
        ]
    }"#;

    let mut child = tsift_bin()
        .args([
            "--envelope",
            "edit-intents",
            "--path",
            dir.path().to_str().unwrap(),
            "--apply",
            "--json",
        ])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    {
        use std::io::Write;
        child
            .stdin
            .as_mut()
            .unwrap()
            .write_all(input.as_bytes())
            .unwrap();
    }
    let output = child.wait_with_output().unwrap();
    assert!(
        output.status.success(),
        "typescript executor stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let json: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(json["report"]["planned_total"], 2);
    assert_eq!(json["report"]["applied_total"], 2);
    assert_eq!(
        json["report"]["plans"][1]["target_symbol"]["language"],
        "typescript"
    );
    assert!(
        json["report"]["plans"][1]["message"]
            .as_str()
            .unwrap()
            .contains("TypeScript semantic edit executor")
    );

    let source = fs::read_to_string(dir.path().join("tool.ts")).unwrap();
    assert!(
        source.contains("import { extra } from \"./extra\";"),
        "{source}"
    );
    assert!(source.contains("return value * 2;"), "{source}");
    assert!(!source.contains("return beta(value);"), "{source}");
}

#[test]
fn edit_intents_apply_mutates_javascript_executor_intents() {
    let dir = script_edit_fixture();
    let input = r#"{
        "intents": [
            {
                "kind": "rename_symbol",
                "symbol": "beta",
                "file": "app.js",
                "new_name": "betaRenamed"
            }
        ]
    }"#;

    let mut child = tsift_bin()
        .args([
            "--envelope",
            "edit-intents",
            "--path",
            dir.path().to_str().unwrap(),
            "--apply",
            "--json",
        ])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    {
        use std::io::Write;
        child
            .stdin
            .as_mut()
            .unwrap()
            .write_all(input.as_bytes())
            .unwrap();
    }
    let output = child.wait_with_output().unwrap();
    assert!(
        output.status.success(),
        "javascript executor stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let json: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(json["report"]["planned_total"], 1);
    assert_eq!(json["report"]["applied_total"], 1);
    assert_eq!(
        json["report"]["plans"][0]["target_symbol"]["language"],
        "javascript"
    );
    assert!(
        json["report"]["plans"][0]["message"]
            .as_str()
            .unwrap()
            .contains("JavaScript semantic edit executor")
    );

    let source = fs::read_to_string(dir.path().join("app.js")).unwrap();
    assert!(source.contains("function betaRenamed(value)"), "{source}");
    assert!(source.contains("return betaRenamed(value);"), "{source}");
    assert!(!source.contains("function beta(value)"), "{source}");
}

#[test]
fn edit_intents_apply_mutates_python_executor_intents() {
    let dir = script_edit_fixture();
    let input = r#"{
        "intents": [
            {
                "kind": "insert_import",
                "file": "script.py",
                "replacement": "from math import sqrt"
            },
            {
                "kind": "replace_function_body",
                "symbol": "alpha",
                "file": "script.py",
                "replacement": "return value * 3"
            }
        ]
    }"#;

    let mut child = tsift_bin()
        .args([
            "--envelope",
            "edit-intents",
            "--path",
            dir.path().to_str().unwrap(),
            "--apply",
            "--json",
        ])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    {
        use std::io::Write;
        child
            .stdin
            .as_mut()
            .unwrap()
            .write_all(input.as_bytes())
            .unwrap();
    }
    let output = child.wait_with_output().unwrap();
    assert!(
        output.status.success(),
        "python executor stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let json: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(json["report"]["planned_total"], 2);
    assert_eq!(json["report"]["applied_total"], 2);
    assert_eq!(
        json["report"]["plans"][1]["target_symbol"]["language"],
        "python"
    );
    assert!(
        json["report"]["plans"][1]["message"]
            .as_str()
            .unwrap()
            .contains("Python semantic edit executor")
    );

    let source = fs::read_to_string(dir.path().join("script.py")).unwrap();
    assert!(source.contains("from math import sqrt"), "{source}");
    assert!(source.contains("return value * 3"), "{source}");
    assert!(!source.contains("return beta(value)"), "{source}");
}

#[test]
fn edit_intents_apply_refuses_typescript_call_rewrite_without_mutating() {
    let dir = script_edit_fixture();
    let before = fs::read_to_string(dir.path().join("tool.ts")).unwrap();
    let input = r#"{
        "intents": [
            {
                "kind": "rewrite_call_sites",
                "symbol": "beta",
                "file": "tool.ts",
                "replacement": "betaRenamed(value)"
            }
        ]
    }"#;

    let mut child = tsift_bin()
        .args([
            "edit-intents",
            "--path",
            dir.path().to_str().unwrap(),
            "--apply",
            "--json",
        ])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    {
        use std::io::Write;
        child
            .stdin
            .as_mut()
            .unwrap()
            .write_all(input.as_bytes())
            .unwrap();
    }
    let output = child.wait_with_output().unwrap();
    assert!(
        !output.status.success(),
        "typescript call rewrite should fail"
    );
    assert_eq!(
        fs::read_to_string(dir.path().join("tool.ts")).unwrap(),
        before
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("unsupported"), "stderr was: {stderr}");
}

#[test]
fn edit_intents_apply_refuses_stale_hash_without_mutating() {
    let dir = indexed_cli_fixture();
    let before = fs::read_to_string(dir.path().join("main.rs")).unwrap();
    let input = r#"{
        "intents": [
            {
                "kind": "rename_symbol",
                "symbol": "alpha",
                "file": "main.rs",
                "new_name": "alpha_renamed",
                "expected_content_hash": "stale"
            }
        ]
    }"#;

    let mut child = tsift_bin()
        .args([
            "edit-intents",
            "--path",
            dir.path().to_str().unwrap(),
            "--apply",
            "--json",
        ])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    {
        use std::io::Write;
        child
            .stdin
            .as_mut()
            .unwrap()
            .write_all(input.as_bytes())
            .unwrap();
    }
    let output = child.wait_with_output().unwrap();
    assert!(!output.status.success(), "stale hash apply should fail");
    assert_eq!(
        fs::read_to_string(dir.path().join("main.rs")).unwrap(),
        before
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("conflict"), "stderr was: {stderr}");
}

#[test]
fn edit_intents_apply_refuses_invalid_parser_output_without_mutating() {
    let dir = indexed_cli_fixture();
    let before = fs::read_to_string(dir.path().join("main.rs")).unwrap();
    let input = r#"{
        "intents": [
            {
                "kind": "replace_function_body",
                "symbol": "beta",
                "file": "main.rs",
                "replacement": "let broken = ;"
            }
        ]
    }"#;

    let mut child = tsift_bin()
        .args([
            "edit-intents",
            "--path",
            dir.path().to_str().unwrap(),
            "--apply",
            "--json",
        ])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    {
        use std::io::Write;
        child
            .stdin
            .as_mut()
            .unwrap()
            .write_all(input.as_bytes())
            .unwrap();
    }
    let output = child.wait_with_output().unwrap();
    assert!(
        !output.status.success(),
        "invalid parser output should fail"
    );
    assert_eq!(
        fs::read_to_string(dir.path().join("main.rs")).unwrap(),
        before
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("replace_function_body produced Rust source with parse errors")
            || stderr.contains("patch proposal output produced Rust source with parse errors"),
        "stderr was: {stderr}"
    );
}

#[test]
fn source_read_json_includes_cached_summary_refs_for_file() {
    let dir = tempfile::tempdir().unwrap();
    fs::write(
        dir.path().join("main.rs"),
        "fn main() {\n    helper();\n}\n\nfn helper() {}\n",
    )
    .unwrap();
    let status = tsift_bin()
        .args(["index", dir.path().to_str().unwrap()])
        .status()
        .unwrap();
    assert!(status.success());
    fs::create_dir_all(dir.path().join(".tsift")).unwrap();
    let conn = Connection::open(dir.path().join(".tsift/summaries.db")).unwrap();
    conn.execute_batch(
        "PRAGMA journal_mode=WAL;
         CREATE TABLE summaries (
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
         );",
    )
    .unwrap();
    conn.execute(
        "INSERT INTO summaries (
            symbol_name, file_path, content_hash, summary, entities, relationships,
            concept_labels, extracted_at, model, tokens_input, tokens_output
        ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
        rusqlite::params![
            "helper",
            "main.rs",
            "hash",
            "helper summary for bounded source reads",
            Option::<String>::None,
            Option::<String>::None,
            Option::<String>::None,
            "1700000000",
            "claude-haiku-4-5-20251001",
            12_i64,
            4_i64
        ],
    )
    .unwrap();

    let output = tsift_bin()
        .args([
            "source-read",
            "main.rs",
            "--path",
            dir.path().to_str().unwrap(),
            "--style",
            "window",
            "--start",
            "1",
            "--lines",
            "5",
            "--json",
        ])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "source-read stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let json: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    let summaries = json["summaries"].as_array().unwrap();
    assert_eq!(summaries.len(), 1);
    assert!(summaries[0]["handle"].as_str().unwrap().starts_with("sum-"));
    assert_eq!(summaries[0]["symbol_name"], "helper");
    assert!(
        summaries[0]["expand"]
            .as_str()
            .unwrap()
            .contains("tsift summarize")
    );
}

#[cfg(unix)]
#[test]
fn index_logs_warning_when_file_read_fails() {
    let dir = tempfile::tempdir().unwrap();
    let main_path = dir.path().join("main.rs");
    fs::write(&main_path, "fn main() {}\n").unwrap();

    let original_mode = fs::metadata(&main_path).unwrap().permissions().mode();
    let mut unreadable = fs::metadata(&main_path).unwrap().permissions();
    unreadable.set_mode(0o000);
    fs::set_permissions(&main_path, unreadable).unwrap();

    let output = tsift_bin()
        .args(["index", dir.path().to_str().unwrap()])
        .output()
        .unwrap();

    let mut restored = fs::metadata(&main_path).unwrap().permissions();
    restored.set_mode(original_mode);
    fs::set_permissions(&main_path, restored).unwrap();

    assert!(output.status.success(), "index should still succeed");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("warning:"), "stderr was: {stderr}");
    assert!(stderr.contains("read failed"), "stderr was: {stderr}");
    assert!(stderr.contains("main.rs"), "stderr was: {stderr}");
}

#[test]
fn search_no_autoindex_fails_fast_when_index_is_stale() {
    let dir = tempfile::tempdir().unwrap();
    fs::write(dir.path().join("main.rs"), "fn main() {}").unwrap();

    let status = tsift_bin()
        .args(["index", dir.path().to_str().unwrap()])
        .status()
        .unwrap();
    assert!(status.success());

    std::thread::sleep(std::time::Duration::from_millis(50));
    fs::write(
        dir.path().join("main.rs"),
        "fn helper() {}\nfn main() { helper(); }",
    )
    .unwrap();

    let output = tsift_bin()
        .args([
            "search",
            "--no-autoindex",
            "--path",
            dir.path().to_str().unwrap(),
            "helper",
        ])
        .output()
        .unwrap();

    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("index is stale"));
    assert!(stderr.contains("--no-autoindex"));
}

#[test]
fn search_autoindex_degrades_to_read_only_when_writer_lock_exists() {
    let dir = tempfile::tempdir().unwrap();
    fs::write(dir.path().join("main.rs"), "fn main() {}").unwrap();

    let status = tsift_bin()
        .args(["index", dir.path().to_str().unwrap()])
        .status()
        .unwrap();
    assert!(status.success());

    std::thread::sleep(std::time::Duration::from_millis(50));
    fs::write(
        dir.path().join("main.rs"),
        "fn helper() {}\nfn main() { helper(); }",
    )
    .unwrap();
    let _lock = hold_writer_lock(&dir.path().join(".tsift/index.lock"));

    let output = tsift_bin()
        .args([
            "search",
            "--autoindex",
            "--path",
            dir.path().to_str().unwrap(),
            "helper",
        ])
        .output()
        .unwrap();

    assert!(output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stderr.contains("active tsift writer detected"));
    assert!(stderr.contains("Continuing with read-only search"));
    assert!(stderr.contains("Retry `tsift index"));
    assert!(stdout.contains("helper"));
}

#[test]
fn search_autoindex_degrades_to_exact_when_writer_lock_blocks_missing_index() {
    let dir = tempfile::tempdir().unwrap();
    fs::write(
        dir.path().join("notes.md"),
        "workspace anchor: live-writer-fallback\n",
    )
    .unwrap();
    fs::create_dir_all(dir.path().join(".tsift")).unwrap();
    let _lock = hold_writer_lock(&dir.path().join(".tsift/index.lock"));

    let output = tsift_bin()
        .args([
            "search",
            "--autoindex",
            "--strategy",
            "lexical",
            "--json",
            "--path",
            dir.path().to_str().unwrap(),
            "live-writer-fallback",
        ])
        .output()
        .unwrap();

    assert!(output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("active tsift writer detected"));
    assert!(stderr.contains("Continuing with exact live-file search"));
    let json: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(json["strategy"], "exact");
    assert_eq!(json["hits"].as_array().unwrap().len(), 1);
    assert!(
        !dir.path().join(".tsift/index.db").exists(),
        "fallback exact search should not synthesize a new index"
    );
}

#[test]
fn search_scope_fails_on_unknown_submodule_name() {
    let dir = tempfile::tempdir().unwrap();
    fs::write(
        dir.path().join(".gitmodules"),
        r#"[submodule "src/alpha"]
	path = src/alpha
	url = https://example.com/alpha
[submodule "src/beta"]
	path = src/beta
	url = https://example.com/beta
"#,
    )
    .unwrap();
    fs::create_dir_all(dir.path().join("src/alpha")).unwrap();
    fs::create_dir_all(dir.path().join("src/beta")).unwrap();
    fs::write(
        dir.path().join("src/alpha/lib.rs"),
        "fn alpha_helper() {}\nfn alpha_main() { alpha_helper(); }\n",
    )
    .unwrap();
    fs::write(dir.path().join("src/beta/lib.rs"), "fn beta_func() {}\n").unwrap();

    let output = tsift_bin()
        .args([
            "search",
            "--scope",
            "missing",
            "--path",
            dir.path().to_str().unwrap(),
            "alpha_main",
        ])
        .output()
        .unwrap();

    assert!(!output.status.success(), "unknown scope should fail closed");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("unknown scope `missing`"),
        "stderr was: {stderr}"
    );
    assert!(
        stderr.contains("Available scopes: alpha, beta"),
        "stderr was: {stderr}"
    );
}

#[test]
fn index_submodule_fails_on_unknown_submodule_name() {
    let dir = tempfile::tempdir().unwrap();
    fs::write(
        dir.path().join(".gitmodules"),
        r#"[submodule "src/alpha"]
	path = src/alpha
	url = https://example.com/alpha
[submodule "src/beta"]
	path = src/beta
	url = https://example.com/beta
"#,
    )
    .unwrap();
    fs::create_dir_all(dir.path().join("src/alpha")).unwrap();
    fs::create_dir_all(dir.path().join("src/beta")).unwrap();
    fs::write(
        dir.path().join("src/alpha/lib.rs"),
        "fn alpha_helper() {}\n",
    )
    .unwrap();
    fs::write(dir.path().join("src/beta/lib.rs"), "fn beta_func() {}\n").unwrap();

    let output = tsift_bin()
        .args([
            "index",
            "--submodule",
            "missing",
            dir.path().to_str().unwrap(),
        ])
        .output()
        .unwrap();

    assert!(
        !output.status.success(),
        "unknown submodule should fail closed"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("unknown scope `missing`"),
        "stderr was: {stderr}"
    );
    assert!(
        stderr.contains("Available scopes: alpha, beta"),
        "stderr was: {stderr}"
    );
    assert!(!dir.path().join(".tsift/indexes/missing/index.db").exists());
}

#[test]
fn search_scope_errors_on_ambiguous_duplicate_leaf_name() {
    let dir = tempfile::tempdir().unwrap();
    fs::write(
        dir.path().join(".gitmodules"),
        r#"[submodule "pkg/app/foo"]
	path = pkg/app/foo
	url = https://example.com/pkg-app-foo
[submodule "vendor/foo"]
	path = vendor/foo
	url = https://example.com/vendor-foo
"#,
    )
    .unwrap();
    fs::create_dir_all(dir.path().join("pkg/app/foo")).unwrap();
    fs::create_dir_all(dir.path().join("vendor/foo")).unwrap();
    fs::write(
        dir.path().join("pkg/app/foo/lib.rs"),
        "fn pkg_only() {}\nfn shared_name() { pkg_only(); }\n",
    )
    .unwrap();
    fs::write(
        dir.path().join("vendor/foo/lib.rs"),
        "fn vendor_only() {}\nfn shared_name() { vendor_only(); }\n",
    )
    .unwrap();

    let output = tsift_bin()
        .args(["index", "--workspace", dir.path().to_str().unwrap()])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "index stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let output = tsift_bin()
        .args([
            "search",
            "--scope",
            "foo",
            "--path",
            dir.path().to_str().unwrap(),
            "vendor_only",
        ])
        .output()
        .unwrap();

    assert!(
        !output.status.success(),
        "ambiguous scope should fail closed"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("ambiguous scope `foo`"),
        "stderr was: {stderr}"
    );
    assert!(stderr.contains("pkg/app/foo"), "stderr was: {stderr}");
    assert!(stderr.contains("vendor/foo"), "stderr was: {stderr}");
}

#[test]
fn status_reports_workspace_scoped_indexes_in_json() {
    let dir = tempfile::tempdir().unwrap();
    fs::write(
        dir.path().join(".gitmodules"),
        r#"[submodule "src/alpha"]
	path = src/alpha
	url = https://example.com/alpha
[submodule "src/beta"]
	path = src/beta
	url = https://example.com/beta
"#,
    )
    .unwrap();
    fs::create_dir_all(dir.path().join("src/alpha")).unwrap();
    fs::create_dir_all(dir.path().join("src/beta")).unwrap();
    fs::write(dir.path().join("src/alpha/lib.rs"), "fn alpha() {}\n").unwrap();
    fs::write(dir.path().join("src/beta/lib.rs"), "fn beta() {}\n").unwrap();

    let output = tsift_bin()
        .args(["index", "--workspace", dir.path().to_str().unwrap()])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "index stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let output = tsift_bin()
        .args(["status", "--json", dir.path().to_str().unwrap()])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "status stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("\"state\":\"fresh\""),
        "stdout was: {stdout}"
    );
    assert!(
        stdout.contains("\"workspace_scopes\""),
        "stdout was: {stdout}"
    );
    assert!(
        stdout.contains("\"scope\":\"alpha\""),
        "stdout was: {stdout}"
    );
    assert!(
        stdout.contains("\"scope\":\"beta\""),
        "stdout was: {stdout}"
    );
    assert!(
        !stdout.contains("\"index\":{\"state\":\"missing\""),
        "stdout was: {stdout}"
    );
}

#[test]
fn status_autoindexes_partially_indexed_workspace_before_reporting_json() {
    let dir = tempfile::tempdir().unwrap();
    fs::write(
        dir.path().join(".gitmodules"),
        r#"[submodule "src/alpha"]
	path = src/alpha
	url = https://example.com/alpha
[submodule "src/beta"]
	path = src/beta
	url = https://example.com/beta
"#,
    )
    .unwrap();
    fs::create_dir_all(dir.path().join("src/alpha")).unwrap();
    fs::create_dir_all(dir.path().join("src/beta")).unwrap();
    fs::write(dir.path().join("src/alpha/lib.rs"), "fn alpha() {}\n").unwrap();
    fs::write(dir.path().join("src/beta/lib.rs"), "fn beta() {}\n").unwrap();

    let output = tsift_bin()
        .args([
            "index",
            "--submodule",
            "alpha",
            dir.path().to_str().unwrap(),
        ])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "index stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let output = tsift_bin()
        .args(["status", "--json", dir.path().to_str().unwrap()])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "status stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("\"state\":\"fresh\""),
        "stdout was: {stdout}"
    );
    assert!(
        stdout.contains("\"workspace_scopes\""),
        "stdout was: {stdout}"
    );
    assert!(
        stdout.contains("\"scope\":\"alpha\""),
        "stdout was: {stdout}"
    );
    assert!(
        stdout.contains("\"scope\":\"beta\""),
        "stdout was: {stdout}"
    );
    assert!(
        !stdout.contains("\"missing_scopes\""),
        "stdout was: {stdout}"
    );
    assert!(dir.path().join(".tsift/indexes/beta/index.db").exists());
}

#[test]
fn status_fix_refreshes_stale_index_before_reporting_json() {
    let dir = indexed_cli_fixture();
    std::thread::sleep(Duration::from_millis(50));
    fs::write(
        dir.path().join("main.rs"),
        "fn helper() { println!(\"updated\"); }\nfn main() { helper(); Vec::new(); }\n",
    )
    .unwrap();

    let output = tsift_bin()
        .args(["status", "--fix", "--json", dir.path().to_str().unwrap()])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "status stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("\"state\":\"fresh\""),
        "stdout was: {stdout}"
    );
    assert!(
        !stdout.contains("\"state\":\"stale\""),
        "stdout was: {stdout}"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("status fix: refreshing index"),
        "stderr was: {stderr}"
    );
}

#[test]
fn status_fix_refreshes_stale_instructions_after_version_bump_in_json() {
    let dir = indexed_cli_fixture();
    fs::write(
        dir.path().join("AGENTS.md"),
        "<!-- tsift:code-navigation v=0.1.41 -->\n## Code Navigation\nOld guidance.\n<!-- /tsift:code-navigation -->\n",
    )
    .unwrap();

    let output = tsift_bin()
        .args(["status", "--fix", "--json", dir.path().to_str().unwrap()])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "status stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("\"instructions\":{\"state\":\"current\""),
        "stdout was: {stdout}"
    );
    assert!(
        stdout.contains(&format!("\"version\":\"{}\"", env!("CARGO_PKG_VERSION"))),
        "stdout was: {stdout}"
    );
    assert!(
        !stdout.contains("\"state\":\"stale\""),
        "stdout was: {stdout}"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("status fix: refreshing tsift instructions"),
        "stderr was: {stderr}"
    );

    let agents = fs::read_to_string(dir.path().join("AGENTS.md")).unwrap();
    assert!(
        agents.contains(&format!(
            "<!-- tsift:code-navigation v={} -->",
            env!("CARGO_PKG_VERSION")
        )),
        "AGENTS.md was: {agents}"
    );
    assert!(
        !agents.contains("v=0.1.41") && !agents.contains("Old guidance."),
        "AGENTS.md was: {agents}"
    );

    // The refreshed block points at the runbook, so the fix must produce it.
    let runbook =
        fs::read_to_string(dir.path().join(".agent/runbooks/code-navigation.md")).unwrap();
    assert!(
        runbook.contains(&format!(
            "<!-- tsift:code-navigation-runbook v={} -->",
            env!("CARGO_PKG_VERSION")
        )),
        "runbook was: {runbook}"
    );
    assert!(
        runbook.contains("report.scale_guard"),
        "runbook was: {runbook}"
    );
}

#[test]
fn init_writes_the_runbook_and_does_not_duplicate_into_a_claude_md_that_imports_agents_md() {
    let dir = tempfile::tempdir().unwrap();
    fs::write(dir.path().join("AGENTS.md"), "# Agents\n").unwrap();
    fs::write(
        dir.path().join("CLAUDE.md"),
        "@AGENTS.md\n\n<!-- tsift:code-navigation v=0.1.41 -->\n## Code Navigation\nOld duplicate.\n<!-- /tsift:code-navigation -->\n\n## Claude extras\n",
    )
    .unwrap();

    let output = tsift_bin()
        .args(["init", dir.path().to_str().unwrap()])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "init stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("duplicate tsift Code Navigation section removed"),
        "stdout was: {stdout}"
    );

    let claude = fs::read_to_string(dir.path().join("CLAUDE.md")).unwrap();
    assert!(
        !claude.contains("tsift:code-navigation"),
        "CLAUDE.md still repeats what it imports: {claude}"
    );
    assert!(claude.contains("@AGENTS.md"), "CLAUDE.md was: {claude}");
    assert!(
        claude.contains("## Claude extras"),
        "CLAUDE.md was: {claude}"
    );

    let agents = fs::read_to_string(dir.path().join("AGENTS.md")).unwrap();
    assert!(
        agents.contains("<!-- tsift:code-navigation v="),
        "AGENTS.md was: {agents}"
    );
    assert!(
        agents.contains(".agent/runbooks/code-navigation.md"),
        "AGENTS.md was: {agents}"
    );
    assert!(
        dir.path()
            .join(".agent/runbooks/code-navigation.md")
            .exists()
    );
}

#[test]
fn status_json_auto_fixes_stale_index_without_fix_flag() {
    let dir = indexed_cli_fixture();
    std::thread::sleep(Duration::from_millis(50));
    fs::write(
        dir.path().join("main.rs"),
        "fn helper() { println!(\"updated\"); }\nfn main() { helper(); Vec::new(); }\n",
    )
    .unwrap();

    let output = tsift_bin()
        .args(["status", "--json", dir.path().to_str().unwrap()])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "status stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("\"state\":\"fresh\""),
        "stdout was: {stdout}"
    );
    assert!(
        !stdout.contains("\"state\":\"stale\""),
        "stdout was: {stdout}"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("status fix: refreshing index"),
        "stderr was: {stderr}"
    );
}

#[test]
fn status_auto_fixes_stale_index_by_default() {
    let dir = indexed_cli_fixture();
    std::thread::sleep(Duration::from_millis(50));
    fs::write(
        dir.path().join("main.rs"),
        "fn helper() { println!(\"updated\"); }\nfn main() { helper(); Vec::new(); }\n",
    )
    .unwrap();

    let output = tsift_bin()
        .args(["status", dir.path().to_str().unwrap()])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "status stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("status fix: refreshing index"),
        "stderr was: {stderr}"
    );
}

#[test]
fn status_no_fix_skips_auto_fix() {
    let dir = indexed_cli_fixture();
    std::thread::sleep(Duration::from_millis(50));
    fs::write(
        dir.path().join("main.rs"),
        "fn helper() { println!(\"updated\"); }\nfn main() { helper(); Vec::new(); }\n",
    )
    .unwrap();

    let output = tsift_bin()
        .args(["status", "--no-fix", "--json", dir.path().to_str().unwrap()])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "status stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("\"state\":\"stale\""),
        "stdout was: {stdout}"
    );
}

#[test]
fn status_deprecated_fix_flag_shows_warning() {
    let dir = indexed_cli_fixture();
    std::thread::sleep(Duration::from_millis(50));
    fs::write(
        dir.path().join("main.rs"),
        "fn helper() { println!(\"updated\"); }\nfn main() { helper(); Vec::new(); }\n",
    )
    .unwrap();

    let output = tsift_bin()
        .args(["status", "--fix", "--json", dir.path().to_str().unwrap()])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "status stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("--fix is deprecated"),
        "stderr was: {stderr}"
    );
}

/// A bare `status` — a read-shaped command — must not leave an unrequested diff
/// in a version-controlled tree, even while it repairs the index it owns.
#[test]
fn status_does_not_rewrite_tracked_instruction_files_by_default() {
    let dir = indexed_cli_fixture();
    let stale_block =
        "<!-- tsift:code-navigation v=0.1.41 -->\n## Code Navigation\nOld guidance.\n<!-- /tsift:code-navigation -->\n";
    fs::write(dir.path().join("AGENTS.md"), stale_block).unwrap();
    std::thread::sleep(Duration::from_millis(50));
    fs::write(
        dir.path().join("main.rs"),
        "fn helper() { println!(\"updated\"); }\nfn main() { helper(); Vec::new(); }\n",
    )
    .unwrap();

    let output = tsift_bin()
        .args(["status", "--json", dir.path().to_str().unwrap()])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "status stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("status fix: refreshing index"),
        "the index it owns is still auto-fixed; stderr was: {stderr}"
    );
    assert!(
        !stderr.contains("refreshing tsift instructions"),
        "bare status must not rewrite tracked files; stderr was: {stderr}"
    );

    assert_eq!(
        fs::read_to_string(dir.path().join("AGENTS.md")).unwrap(),
        stale_block,
        "AGENTS.md must be byte-identical after a bare status"
    );
    assert!(
        !dir.path().join(".agent/runbooks/code-navigation.md").exists(),
        "bare status must not create the managed runbook"
    );

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("\"instructions\":{\"state\":\"stale\""),
        "stale instructions must still be reported; stdout was: {stdout}"
    );
}

#[test]
fn status_fix_instructions_names_every_tracked_file_it_writes() {
    let dir = indexed_cli_fixture();
    fs::write(
        dir.path().join("AGENTS.md"),
        "<!-- tsift:code-navigation v=0.1.41 -->\n## Code Navigation\nOld guidance.\n<!-- /tsift:code-navigation -->\n",
    )
    .unwrap();

    let output = tsift_bin()
        .args([
            "status",
            "--fix-instructions",
            "--json",
            dir.path().to_str().unwrap(),
        ])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "status stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("status fix: rewrote AGENTS.md (v0.1.41 -> v"),
        "stderr was: {stderr}"
    );
    assert!(
        stderr.contains(&format!(
            "status fix: created .agent/runbooks/code-navigation.md (v0.1.41 -> v{})",
            env!("CARGO_PKG_VERSION")
        )),
        "stderr was: {stderr}"
    );

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("\"instructions\":{\"state\":\"current\""),
        "stdout was: {stdout}"
    );
}

#[test]
fn status_fix_instructions_names_the_legacy_runbook_relocation() {
    let dir = indexed_cli_fixture();
    fs::write(
        dir.path().join("AGENTS.md"),
        "<!-- tsift:code-navigation v=0.1.41 -->\n## Code Navigation\nOld guidance.\n<!-- /tsift:code-navigation -->\n",
    )
    .unwrap();
    fs::create_dir_all(dir.path().join("runbooks")).unwrap();
    fs::write(
        dir.path().join("runbooks/code-navigation.md"),
        "# Code Navigation\n\nHand-written trailer.\n",
    )
    .unwrap();

    let output = tsift_bin()
        .args([
            "status",
            "--fix-instructions",
            "--json",
            dir.path().to_str().unwrap(),
        ])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "status stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains(
            "status fix: moved runbooks/code-navigation.md -> .agent/runbooks/code-navigation.md"
        ),
        "a tracked deletion must be named, not silent; stderr was: {stderr}"
    );
    assert!(!dir.path().join("runbooks/code-navigation.md").exists());
    let moved =
        fs::read_to_string(dir.path().join(".agent/runbooks/code-navigation.md")).unwrap();
    assert!(
        moved.contains("Hand-written trailer."),
        "unmanaged text must survive the move; runbook was: {moved}"
    );
}

#[test]
fn init_names_the_legacy_runbook_relocation() {
    let dir = tempfile::tempdir().unwrap();
    fs::write(dir.path().join("AGENTS.md"), "# Agents\n").unwrap();
    fs::create_dir_all(dir.path().join("runbooks")).unwrap();
    fs::write(
        dir.path().join("runbooks/code-navigation.md"),
        "# Code Navigation\n",
    )
    .unwrap();

    let output = tsift_bin()
        .args(["init", dir.path().to_str().unwrap()])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "init stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("runbooks/code-navigation.md: moved -> .agent/runbooks/code-navigation.md"),
        "stdout was: {stdout}"
    );
}

/// The instructions tsift writes must not teach a flag tsift deprecated.
#[test]
fn generated_code_navigation_block_does_not_recommend_the_deprecated_status_fix_flag() {
    let dir = tempfile::tempdir().unwrap();
    let output = tsift_bin()
        .args(["init", dir.path().to_str().unwrap()])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "init stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let agents = fs::read_to_string(dir.path().join("AGENTS.md")).unwrap();
    let runbook =
        fs::read_to_string(dir.path().join(".agent/runbooks/code-navigation.md")).unwrap();
    for surface in [&agents, &runbook] {
        assert!(
            !surface.contains("tsift status --fix"),
            "generated instructions still teach the deprecated flag: {surface}"
        );
    }
    assert!(
        agents.contains("`tsift init` to refresh the tracked Code Navigation block"),
        "AGENTS.md was: {agents}"
    );
}

#[test]
fn status_autoindexes_missing_workspace_scopes_even_when_root_index_exists_in_json() {
    let dir = tempfile::tempdir().unwrap();
    fs::write(
        dir.path().join(".gitmodules"),
        r#"[submodule "src/alpha"]
	path = src/alpha
	url = https://example.com/alpha
[submodule "src/beta"]
	path = src/beta
	url = https://example.com/beta
"#,
    )
    .unwrap();
    fs::create_dir_all(dir.path().join("src/alpha")).unwrap();
    fs::create_dir_all(dir.path().join("src/beta")).unwrap();
    fs::write(dir.path().join("src/alpha/lib.rs"), "fn alpha() {}\n").unwrap();
    fs::write(dir.path().join("src/beta/lib.rs"), "fn beta() {}\n").unwrap();

    let output = tsift_bin()
        .args(["index", dir.path().to_str().unwrap()])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "root index stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let output = tsift_bin()
        .args([
            "index",
            "--submodule",
            "alpha",
            dir.path().to_str().unwrap(),
        ])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "scoped index stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let output = tsift_bin()
        .args(["status", "--json", dir.path().to_str().unwrap()])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "status stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("\"state\":\"fresh\""),
        "stdout was: {stdout}"
    );
    assert!(stdout.contains("\"total_files\":2"), "stdout was: {stdout}");
    assert!(
        stdout.contains("\"workspace_scopes\""),
        "stdout was: {stdout}"
    );
    assert!(
        stdout.contains("\"scope\":\"alpha\""),
        "stdout was: {stdout}"
    );
    assert!(
        stdout.contains("\"scope\":\"beta\""),
        "stdout was: {stdout}"
    );
    assert!(
        !stdout.contains("\"missing_scopes\""),
        "stdout was: {stdout}"
    );
    assert!(dir.path().join(".tsift/indexes/beta/index.db").exists());
}

#[test]
fn workspace_graph_queries_resolve_scopes_without_shared_root_index() {
    let dir = indexed_workspace_cli_fixture();
    let root = dir.path().to_str().unwrap();
    // #graphfed and #wsfedrest emptied this set: every read-only graph command
    // now resolves the workspace itself. `path` is the one that can still
    // refuse, and it refuses for a reason no flag would fix — see
    // `workspace_path_refuses_endpoints_in_different_scopes`.
    let output = tsift_bin()
        .args(["path", "alpha_main", "beta_func", root, "--json"])
        .output()
        .unwrap();
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        !stderr.contains("no index found at"),
        "the refusal must be about scopes, not a missing index: {stderr}"
    );
}

// #wsfedrest: a scoped index holds only its own call edges, so per-scope
// community detection is the exact answer rather than an approximation of a
// whole-workspace one — which is why `communities` federates by concatenation.
#[test]
fn workspace_communities_federates_per_scope() {
    let dir = indexed_workspace_cli_fixture();
    let root = dir.path().to_str().unwrap();

    let output = tsift_bin()
        .args(["communities", root, "--json"])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "communities stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let json: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    let scopes = json["scopes"]
        .as_array()
        .expect("a federated run reports one document per scope");
    assert_eq!(scopes.len(), 2, "{json}");
    let ids: Vec<&str> = scopes
        .iter()
        .map(|entry| entry["scope"].as_str().unwrap())
        .collect();
    assert_eq!(ids, vec!["alpha", "beta"], "{json}");

    // Human output labels each scope rather than silently concatenating.
    let human = tsift_bin().args(["communities", root]).output().unwrap();
    assert!(human.status.success());
    let text = String::from_utf8_lossy(&human.stdout);
    assert!(text.contains("scope alpha:") && text.contains("scope beta:"), "{text}");
}

// #wsfedrest: `path` resolves both endpoints. Same scope is answerable; a
// cross-scope pair is a precise refusal, because no edge could cross between two
// scoped indexes — an empty result would read as "no path in a graph that has
// both", which is not what happened.
#[test]
fn workspace_path_refuses_endpoints_in_different_scopes() {
    let dir = indexed_workspace_cli_fixture();
    let root = dir.path().to_str().unwrap();

    let same_scope = tsift_bin()
        .args(["path", "alpha_main", "alpha_helper", root, "--json"])
        .output()
        .unwrap();
    assert!(
        same_scope.status.success(),
        "both endpoints in scope alpha must be answerable: {}",
        String::from_utf8_lossy(&same_scope.stderr)
    );

    let cross_scope = tsift_bin()
        .args(["path", "alpha_main", "beta_func", root, "--json"])
        .output()
        .unwrap();
    assert!(!cross_scope.status.success());
    let stderr = String::from_utf8_lossy(&cross_scope.stderr);
    assert!(stderr.contains("scope `alpha`"), "{stderr}");
    assert!(stderr.contains("scope `beta`"), "{stderr}");
    assert!(stderr.contains("no path between them exists"), "{stderr}");

    for command in ["communities", "path"] {
        let help = tsift_bin().args([command, "--help"]).output().unwrap();
        let text = String::from_utf8_lossy(&help.stdout);
        assert!(
            text.contains("--federated"),
            "`{command} --help` must offer --federated: {text}"
        );
    }
}

#[test]
// #wsfed regression: whether plain `tsift search <query>` worked at a workspace
// root used to depend on the *shape* of the query. An identifier routed to the
// `exact` strategy and federated fine; anything falling through to
// `fts`/`lexical` exited 1 demanding `--scope` or `--federated`. Same directory,
// same command, same flags.
fn workspace_search_federates_by_default_without_shared_root_index() {
    let dir = indexed_workspace_cli_fixture();
    let root = dir.path().to_str().unwrap();

    // The query shape that used to fail: no underscore, routes past `exact`.
    for query in ["helper", "alpha_helper"] {
        let output = tsift_bin()
            .args(["search", query, "--path", root, "--json"])
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "`search {query}` at a workspace root must federate rather than fail: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    let output = tsift_bin()
        .args(["search", "alpha_helper", "--path", root, "--json"])
        .output()
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    let rendered = json.to_string();
    assert!(
        rendered.contains("alpha_helper"),
        "federated search must reach the scope that owns the symbol: {rendered}"
    );
    assert!(!dir.path().join(".tsift/index.db").exists());
}

// #wsinit regression: `tsift init --workspace` refreshed instruction files only
// in the superproject while `status` maintained index state for every scope, so
// submodules stayed on releases-old text — and AGENTS.md tells an agent to work
// from the submodule root, which is exactly the file that never got refreshed.
#[test]
fn init_workspace_refreshes_every_scope_instruction_surface() {
    let dir = indexed_workspace_cli_fixture();
    let root = dir.path().to_str().unwrap();

    let output = tsift_bin().args(["init", "--workspace", root]).output().unwrap();
    assert!(
        output.status.success(),
        "init --workspace stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("scope alpha:"), "{stdout}");
    assert!(stdout.contains("scope beta:"), "{stdout}");

    for scope in ["alpha", "beta"] {
        let agents = dir.path().join(format!("src/{scope}/AGENTS.md"));
        assert!(
            agents.exists(),
            "init --workspace must write {}",
            agents.display()
        );
        let runbook = dir
            .path()
            .join(format!("src/{scope}/.agent/runbooks/code-navigation.md"));
        assert!(
            runbook.exists(),
            "init --workspace must write {}",
            runbook.display()
        );
    }

    // The freshly-inited workspace no longer reports scope instruction drift.
    let status = tsift_bin().args(["status", "--json", root]).output().unwrap();
    let json: serde_json::Value = serde_json::from_slice(&status.stdout).unwrap();
    let drifted = json["scope_instructions"]
        .as_array()
        .map(|scopes| {
            scopes
                .iter()
                .filter(|scope| scope["instructions"]["state"] != "current")
                .count()
        })
        .unwrap_or(0);
    assert_eq!(drifted, 0, "{json}");
}

#[test]
fn init_workspace_skips_scopes_that_opt_out_of_instructions() {
    let dir = indexed_workspace_cli_fixture();
    let root = dir.path().to_str().unwrap();
    fs::create_dir_all(dir.path().join(".tsift")).unwrap();
    fs::write(
        dir.path().join(".tsift/config.toml"),
        "[overrides.alpha]\ninstructions = false\n",
    )
    .unwrap();

    let output = tsift_bin().args(["init", "--workspace", root]).output().unwrap();
    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("scope alpha: skipped"), "{stdout}");
    assert!(
        !dir.path().join("src/alpha/AGENTS.md").exists(),
        "an opted-out scope must not be written"
    );
    assert!(dir.path().join("src/beta/AGENTS.md").exists());
}

// #graphfed regression: `search` had `--federated`, `explain` and `graph` did
// not, so at a workspace root the two graph commands could not run at all
// unless the caller already knew which scope held the symbol — the thing they
// were about to ask tsift.
#[test]
fn workspace_explain_and_graph_resolve_the_owning_scope_without_a_flag() {
    let dir = indexed_workspace_cli_fixture();
    let root = dir.path().to_str().unwrap();

    let explain = tsift_bin()
        .args(["explain", "alpha_helper", root, "--json"])
        .output()
        .unwrap();
    assert!(
        explain.status.success(),
        "explain at a workspace root must resolve the scope: {}",
        String::from_utf8_lossy(&explain.stderr)
    );
    let explain_json: serde_json::Value = serde_json::from_slice(&explain.stdout).unwrap();
    assert!(
        explain_json.to_string().contains("alpha_helper"),
        "{explain_json}"
    );

    let graph = tsift_bin()
        .args(["graph", "alpha_helper", root, "--callers", "--json"])
        .output()
        .unwrap();
    assert!(
        graph.status.success(),
        "graph at a workspace root must resolve the scope: {}",
        String::from_utf8_lossy(&graph.stderr)
    );
    let graph_json: serde_json::Value = serde_json::from_slice(&graph.stdout).unwrap();
    assert!(
        graph_json.to_string().contains("alpha_main"),
        "the caller lives in the resolved scope: {graph_json}"
    );

    // The explicit flag is accepted too, and both commands advertise it.
    for command in ["explain", "graph"] {
        let help = tsift_bin().args([command, "--help"]).output().unwrap();
        let text = String::from_utf8_lossy(&help.stdout);
        assert!(
            text.contains("--federated"),
            "`{command} --help` must offer --federated: {text}"
        );
    }
}

// A symbol no scope defines must say which scopes were searched instead of
// telling the caller to supply a scope it could have resolved itself.
#[test]
fn workspace_explain_names_the_searched_scopes_when_the_symbol_is_absent() {
    let dir = indexed_workspace_cli_fixture();
    let root = dir.path().to_str().unwrap();

    let output = tsift_bin()
        .args(["explain", "no_such_symbol_anywhere", root, "--json"])
        .output()
        .unwrap();
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("was not found in any federated scope"),
        "{stderr}"
    );
    assert!(stderr.contains("alpha") && stderr.contains("beta"), "{stderr}");
}

#[test]
fn nested_query_paths_use_the_ancestor_tsift_root() {
    let dir = indexed_cli_fixture();
    fs::create_dir_all(dir.path().join("src/nested")).unwrap();
    let nested = dir.path().join("src");
    let nested_str = nested.to_str().unwrap();

    let status_output = tsift_bin()
        .args(["status", "--json", nested_str])
        .output()
        .unwrap();
    assert!(
        status_output.status.success(),
        "status stderr: {}",
        String::from_utf8_lossy(&status_output.stderr)
    );
    let status_json: serde_json::Value = serde_json::from_slice(&status_output.stdout).unwrap();
    assert_eq!(status_json["index"]["state"], "fresh");

    let search_output = tsift_bin()
        .args(["search", "--path", nested_str, "helper", "--json"])
        .output()
        .unwrap();
    assert!(
        search_output.status.success(),
        "search stderr: {}",
        String::from_utf8_lossy(&search_output.stderr)
    );
    assert!(
        !nested.join(".tsift/index.db").exists(),
        "search should not create a nested index under {}",
        nested.display()
    );

    let graph_output = tsift_bin()
        .args(["graph", "helper", nested_str, "--json"])
        .output()
        .unwrap();
    assert!(
        graph_output.status.success(),
        "graph stderr: {}",
        String::from_utf8_lossy(&graph_output.stderr)
    );
}

#[test]
fn index_check_stays_read_only_while_writer_lock_exists() {
    let dir = tempfile::tempdir().unwrap();
    fs::write(dir.path().join("main.rs"), "fn main() {}").unwrap();

    let status = tsift_bin()
        .args(["index", dir.path().to_str().unwrap()])
        .status()
        .unwrap();
    assert!(status.success());

    let _lock = hold_writer_lock(&dir.path().join(".tsift/index.lock"));

    let status = tsift_bin()
        .args([
            "index",
            "--check",
            "--exit-code",
            dir.path().to_str().unwrap(),
        ])
        .status()
        .unwrap();
    assert!(status.success(), "expected check mode to stay read-only");
}

#[test]
fn communities_stays_read_only_while_writer_lock_exists() {
    let dir = tempfile::tempdir().unwrap();
    build_cli_fixture(dir.path());

    let status = tsift_bin()
        .args(["index", dir.path().to_str().unwrap()])
        .status()
        .unwrap();
    assert!(status.success());

    let _lock = hold_writer_lock(&dir.path().join(".tsift/index.lock"));

    let status = tsift_bin()
        .args(["communities", dir.path().to_str().unwrap(), "--json"])
        .status()
        .unwrap();
    assert!(
        status.success(),
        "expected communities to stay read-only while a writer lock exists"
    );
}

#[test]
fn status_stays_read_only_while_live_wal_writer_holds_index_db() {
    let dir = indexed_cli_fixture();
    let _lock = hold_wal_lock(&dir.path().join(".tsift/index.db"));

    let output = tsift_bin()
        .args(["status", "--json", dir.path().to_str().unwrap()])
        .output()
        .unwrap();

    assert!(
        output.status.success(),
        "status stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("\"recovery\":\"snapshot_fallback_wal\""));
}

#[test]
fn summarize_stats_stays_read_only_while_live_wal_writer_holds_summary_db() {
    let dir = indexed_cli_fixture();
    create_summary_cache(dir.path());
    let _lock = hold_wal_lock(&dir.path().join(".tsift/summaries.db"));

    let output = tsift_bin()
        .args([
            "summarize",
            "--stats",
            "--path",
            dir.path().to_str().unwrap(),
        ])
        .output()
        .unwrap();

    assert!(
        output.status.success(),
        "summarize stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("Summary cache statistics:"));
    assert!(stdout.contains("files:           1"));
}

#[test]
fn lint_auto_discovers_root_index_db() {
    let dir = tempfile::tempdir().unwrap();
    fs::write(dir.path().join("main.rs"), "fn alpha_helper() {}\n").unwrap();
    fs::write(
        dir.path().join("README.md"),
        "alpha_helper should be backticked.\n",
    )
    .unwrap();

    let output = tsift_bin()
        .args(["index", dir.path().to_str().unwrap()])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "index stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let output = tsift_bin()
        .current_dir(dir.path())
        .args(["lint", "README.md", "--json"])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "lint stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let json: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    let annotations = json["annotations"].as_array().unwrap();
    assert!(
        annotations
            .iter()
            .any(|ann| ann["text"].as_str() == Some("alpha_helper")),
        "stdout was: {}",
        String::from_utf8_lossy(&output.stdout)
    );
}

#[test]
fn lint_auto_discovery_skips_non_federated_workspace_scopes() {
    let dir = tempfile::tempdir().unwrap();
    fs::write(
        dir.path().join(".gitmodules"),
        r#"[submodule "src/public"]
	path = src/public
	url = https://example.com/public
[submodule "src/private"]
	path = src/private
	url = https://example.com/private
[submodule "src/isolated"]
	path = src/isolated
	url = https://example.com/isolated
"#,
    )
    .unwrap();
    fs::create_dir_all(dir.path().join(".tsift")).unwrap();
    fs::write(
        dir.path().join(".tsift/config.toml"),
        r#"
[overrides.private]
tier = "private"

[overrides.isolated]
tier = "isolated"
"#,
    )
    .unwrap();
    fs::create_dir_all(dir.path().join("src/public")).unwrap();
    fs::create_dir_all(dir.path().join("src/private")).unwrap();
    fs::create_dir_all(dir.path().join("src/isolated")).unwrap();
    fs::write(
        dir.path().join("src/public/lib.rs"),
        "fn public_helper() {}\n",
    )
    .unwrap();
    fs::write(
        dir.path().join("src/private/lib.rs"),
        "fn private_helper() {}\n",
    )
    .unwrap();
    fs::write(
        dir.path().join("src/isolated/lib.rs"),
        "fn isolated_helper() {}\n",
    )
    .unwrap();
    fs::write(
        dir.path().join("README.md"),
        "public_helper should be backticked.\nprivate_helper should stay hidden.\nisolated_helper should stay hidden.\n",
    )
    .unwrap();

    let output = tsift_bin()
        .args(["index", "--workspace", dir.path().to_str().unwrap()])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "index stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let output = tsift_bin()
        .current_dir(dir.path())
        .args(["lint", "README.md", "--json"])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "lint stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let json: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    let annotations = json["annotations"].as_array().unwrap();
    assert!(
        annotations
            .iter()
            .any(|ann| ann["text"].as_str() == Some("public_helper")),
        "stdout was: {}",
        String::from_utf8_lossy(&output.stdout)
    );
    assert!(
        annotations
            .iter()
            .all(|ann| ann["text"].as_str() != Some("private_helper")),
        "stdout was: {}",
        String::from_utf8_lossy(&output.stdout)
    );
    assert!(
        annotations
            .iter()
            .all(|ann| ann["text"].as_str() != Some("isolated_helper")),
        "stdout was: {}",
        String::from_utf8_lossy(&output.stdout)
    );
}

#[test]
fn lint_accepts_explicit_indexes_dir() {
    let dir = tempfile::tempdir().unwrap();
    fs::write(
        dir.path().join(".gitmodules"),
        r#"[submodule "src/alpha"]
	path = src/alpha
	url = https://example.com/alpha
"#,
    )
    .unwrap();
    fs::create_dir_all(dir.path().join("src/alpha")).unwrap();
    fs::write(
        dir.path().join("src/alpha/lib.rs"),
        "fn alpha_helper() {}\nfn alpha_main() { alpha_helper(); }\n",
    )
    .unwrap();
    fs::write(
        dir.path().join("README.md"),
        "alpha_helper should be backticked.\n",
    )
    .unwrap();

    let output = tsift_bin()
        .args(["index", "--workspace", dir.path().to_str().unwrap()])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "index stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let output = tsift_bin()
        .current_dir(dir.path())
        .args(["lint", "README.md", "--index", ".tsift/indexes", "--json"])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "lint stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let json: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    let annotations = json["annotations"].as_array().unwrap();
    assert!(
        annotations
            .iter()
            .any(|ann| ann["text"].as_str() == Some("alpha_helper")),
        "stdout was: {}",
        String::from_utf8_lossy(&output.stdout)
    );
}

#[test]
fn lint_accepts_explicit_indexes_dir_with_nested_scope_ids() {
    let dir = tempfile::tempdir().unwrap();
    fs::write(
        dir.path().join(".gitmodules"),
        r#"[submodule "pkg/app/foo"]
	path = pkg/app/foo
	url = https://example.com/pkg-foo
[submodule "vendor/foo"]
	path = vendor/foo
	url = https://example.com/vendor-foo
"#,
    )
    .unwrap();
    fs::create_dir_all(dir.path().join("pkg/app/foo")).unwrap();
    fs::create_dir_all(dir.path().join("vendor/foo")).unwrap();
    fs::create_dir_all(dir.path().join("exported/indexes/pkg/app/foo")).unwrap();
    fs::create_dir_all(dir.path().join("exported/indexes/vendor/foo")).unwrap();
    fs::write(
        dir.path().join("pkg/app/foo/lib.rs"),
        "fn pkg_helper() {}\nfn pkg_main() { pkg_helper(); }\n",
    )
    .unwrap();
    fs::write(
        dir.path().join("vendor/foo/lib.rs"),
        "fn vendor_helper() {}\nfn vendor_main() { vendor_helper(); }\n",
    )
    .unwrap();
    fs::write(
        dir.path().join("README.md"),
        "pkg_helper and vendor_helper should be backticked.\n",
    )
    .unwrap();

    let output = tsift_bin()
        .args(["index", "--workspace", dir.path().to_str().unwrap()])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "index stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    fs::copy(
        dir.path().join(".tsift/indexes/pkg/app/foo/index.db"),
        dir.path().join("exported/indexes/pkg/app/foo/index.db"),
    )
    .unwrap();
    fs::copy(
        dir.path().join(".tsift/indexes/vendor/foo/index.db"),
        dir.path().join("exported/indexes/vendor/foo/index.db"),
    )
    .unwrap();

    let output = tsift_bin()
        .current_dir(dir.path())
        .args(["lint", "README.md", "--index", "exported/indexes", "--json"])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "lint stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let json: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    let annotations = json["annotations"].as_array().unwrap();
    assert!(
        annotations
            .iter()
            .any(|ann| ann["text"].as_str() == Some("pkg_helper")),
        "stdout was: {}",
        String::from_utf8_lossy(&output.stdout)
    );
    assert!(
        annotations
            .iter()
            .any(|ann| ann["text"].as_str() == Some("vendor_helper")),
        "stdout was: {}",
        String::from_utf8_lossy(&output.stdout)
    );
}

#[test]
fn lint_ignores_repo_root_index_db_for_workspace_aggregate_discovery() {
    let dir = tempfile::tempdir().unwrap();
    fs::write(
        dir.path().join(".gitmodules"),
        r#"[submodule "src/alpha"]
	path = src/alpha
	url = https://example.com/alpha
"#,
    )
    .unwrap();
    fs::create_dir_all(dir.path().join("src/alpha")).unwrap();
    fs::write(
        dir.path().join("src/alpha/lib.rs"),
        "fn alpha_helper() {}\nfn alpha_main() { alpha_helper(); }\n",
    )
    .unwrap();
    fs::write(
        dir.path().join("README.md"),
        "alpha_helper should be backticked.\n",
    )
    .unwrap();

    let output = tsift_bin()
        .args(["index", "--workspace", dir.path().to_str().unwrap()])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "index stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let conn = Connection::open(dir.path().join("index.db")).unwrap();
    conn.execute_batch("CREATE TABLE unrelated (id INTEGER PRIMARY KEY);")
        .unwrap();
    drop(conn);

    let output = tsift_bin()
        .current_dir(dir.path())
        .args(["lint", "README.md", "--json"])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "lint stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let json: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    let annotations = json["annotations"].as_array().unwrap();
    assert!(
        annotations
            .iter()
            .any(|ann| ann["text"].as_str() == Some("alpha_helper")),
        "stdout was: {}",
        String::from_utf8_lossy(&output.stdout)
    );
}

#[test]
fn lint_stays_read_only_while_rollback_journal_lock_exists() {
    let dir = tempfile::tempdir().unwrap();
    fs::write(dir.path().join("main.rs"), "fn alpha_helper() {}\n").unwrap();
    fs::write(
        dir.path().join("README.md"),
        "alpha_helper should be backticked.\n",
    )
    .unwrap();

    let output = tsift_bin()
        .args(["index", dir.path().to_str().unwrap()])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "index stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let _lock = hold_rollback_journal_lock(&dir.path().join(".tsift/index.db"));

    let output = tsift_bin()
        .current_dir(dir.path())
        .args(["lint", "README.md", "--json"])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "lint stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let json: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    let annotations = json["annotations"].as_array().unwrap();
    assert!(
        annotations
            .iter()
            .any(|ann| ann["text"].as_str() == Some("alpha_helper")),
        "stdout was: {}",
        String::from_utf8_lossy(&output.stdout)
    );
}

#[test]
fn summarize_stats_fails_closed_when_cache_missing() {
    let dir = tempfile::tempdir().unwrap();

    let output = tsift_bin()
        .args([
            "summarize",
            "--stats",
            "--path",
            dir.path().to_str().unwrap(),
        ])
        .output()
        .unwrap();

    assert!(
        !output.status.success(),
        "expected summarize --stats to fail when cache is missing"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("no summaries.db found"),
        "stderr was: {stderr}"
    );
    assert!(!dir.path().join(".tsift/summaries.db").exists());
}

#[test]
fn summarize_stats_reports_real_stale_counts() {
    let dir = tempfile::tempdir().unwrap();
    fs::create_dir_all(dir.path().join("src")).unwrap();
    fs::write(
        dir.path().join("src/lib.rs"),
        "fn alpha_helper() { changed(); }\n",
    )
    .unwrap();
    create_summary_cache(dir.path());

    let conn = Connection::open(dir.path().join(".tsift/summaries.db")).unwrap();
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
            "missing_helper",
            "src/missing.rs",
            "missing-hash",
            "stale summary",
            Option::<String>::None,
            Option::<String>::None,
            Option::<String>::None,
            "1700000000",
            "claude-haiku-4-5-20251001",
            100_i64,
            40_i64
        ],
    )
    .unwrap();

    let output = tsift_bin()
        .args([
            "summarize",
            "--stats",
            "--path",
            dir.path().to_str().unwrap(),
            "--json",
        ])
        .output()
        .unwrap();

    assert!(
        output.status.success(),
        "summarize stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let json: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(json["total_files"], 2);
    assert_eq!(json["stale_count"], 2);
}

#[test]
fn summarize_stats_treats_out_of_root_cache_keys_as_stale() {
    let dir = tempfile::tempdir().unwrap();
    fs::create_dir_all(dir.path().join("src")).unwrap();
    fs::write(dir.path().join("src/lib.rs"), "fn alpha_helper() {}\n").unwrap();
    create_summary_cache(dir.path());

    let escaped = dir.path().join("..").join("secret.rs");
    fs::write(&escaped, "fn secret() {}\n").unwrap();

    let conn = Connection::open(dir.path().join(".tsift/summaries.db")).unwrap();
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
            "escaped_helper",
            "../secret.rs",
            "secret-hash",
            "escaped summary",
            Option::<String>::None,
            Option::<String>::None,
            Option::<String>::None,
            "1700000000",
            "claude-haiku-4-5-20251001",
            100_i64,
            40_i64
        ],
    )
    .unwrap();

    let output = tsift_bin()
        .args([
            "summarize",
            "--stats",
            "--path",
            dir.path().to_str().unwrap(),
            "--json",
        ])
        .output()
        .unwrap();

    assert!(
        output.status.success(),
        "summarize stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let json: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(json["total_files"], 2);
    assert_eq!(json["stale_count"], 2);
}

#[cfg(unix)]
#[test]
fn summarize_stats_warns_and_succeeds_when_source_is_unreadable() {
    let dir = tempfile::tempdir().unwrap();
    fs::create_dir_all(dir.path().join("src")).unwrap();
    let file_path = dir.path().join("src/lib.rs");
    fs::write(&file_path, "fn alpha_helper() {}\n").unwrap();
    create_summary_cache(dir.path());

    let content = fs::read(&file_path).unwrap();
    let conn = Connection::open(dir.path().join(".tsift/summaries.db")).unwrap();
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
            "alpha_helper",
            "src/lib.rs",
            blake3::hash(&content).to_hex().to_string(),
            "cached summary",
            Option::<String>::None,
            Option::<String>::None,
            Option::<String>::None,
            "1700000000",
            "claude-haiku-4-5-20251001",
            100_i64,
            40_i64
        ],
    )
    .unwrap();

    let metadata = fs::metadata(&file_path).unwrap();
    let original_mode = metadata.permissions().mode();
    let mut unreadable = metadata.permissions();
    unreadable.set_mode(0o000);
    fs::set_permissions(&file_path, unreadable).unwrap();

    let output = tsift_bin()
        .args([
            "summarize",
            "--stats",
            "--path",
            dir.path().to_str().unwrap(),
            "--json",
        ])
        .output()
        .unwrap();

    let mut restored = fs::metadata(&file_path).unwrap().permissions();
    restored.set_mode(original_mode);
    fs::set_permissions(&file_path, restored).unwrap();

    assert!(
        output.status.success(),
        "summarize stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let json: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(json["stale_count"], 1);
    let warnings = json["warnings"].as_array().expect("warnings array");
    assert_eq!(warnings.len(), 1);
    assert_eq!(warnings[0]["path"], "src/lib.rs");

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("warning: summarize stats src/lib.rs:"),
        "stderr was: {stderr}"
    );
    assert!(
        stderr
            .contains("counting cached summary as stale because the source file could not be read"),
        "stderr was: {stderr}"
    );
}

#[test]
fn summarize_extract_resolves_relative_path_against_explicit_root() {
    let project = tempfile::tempdir().unwrap();
    fs::create_dir_all(project.path().join("src")).unwrap();
    fs::write(project.path().join("src/main.rs"), "fn alpha_helper() {}\n").unwrap();

    let caller = tempfile::tempdir().unwrap();
    fs::create_dir_all(caller.path().join("src")).unwrap();

    let mut command = tsift_bin();
    let output = mock_anthropic_extraction(&mut command)
        .current_dir(caller.path())
        .args([
            "summarize",
            "--extract",
            "src",
            "--path",
            project.path().to_str().unwrap(),
            "--compact",
        ])
        .output()
        .unwrap();

    assert!(
        output.status.success(),
        "summarize stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        !stdout.contains("No files to extract."),
        "stdout was: {stdout}"
    );
    assert!(stdout.contains("files:1"), "stdout was: {stdout}");
    assert!(stdout.contains("errors:0"), "stdout was: {stdout}");
    assert!(!caller.path().join(".tsift/summaries.db").exists());
    assert!(project.path().join(".tsift/summaries.db").exists());
}

#[test]
fn summarize_extract_uses_nested_path_as_relative_extract_anchor() {
    let project = tempfile::tempdir().unwrap();
    fs::create_dir_all(project.path().join(".tsift")).unwrap();
    fs::create_dir_all(project.path().join("src")).unwrap();
    fs::create_dir_all(project.path().join("src/nested")).unwrap();
    fs::write(project.path().join("src/main.rs"), "fn root_helper() {}\n").unwrap();
    fs::write(
        project.path().join("src/nested/main.rs"),
        "fn nested_helper() {}\n",
    )
    .unwrap();

    let nested = project.path().join("src/nested");
    let mut command = tsift_bin();
    let output = mock_anthropic_extraction(&mut command)
        .current_dir(project.path())
        .args([
            "summarize",
            "--extract",
            ".",
            "--path",
            nested.to_str().unwrap(),
            "--compact",
        ])
        .output()
        .unwrap();

    assert!(
        output.status.success(),
        "summarize stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        !stdout.contains("No files to extract."),
        "stdout was: {stdout}"
    );
    assert!(stdout.contains("files:1"), "stdout was: {stdout}");
    assert!(stdout.contains("errors:0"), "stdout was: {stdout}");
    assert!(!nested.join(".tsift/summaries.db").exists());
    assert!(project.path().join(".tsift/summaries.db").exists());
}

#[test]
fn summarize_extract_missing_credentials_fails_once_before_walking_files() {
    let dir = tempfile::tempdir().unwrap();
    fs::create_dir_all(dir.path().join("src")).unwrap();
    fs::write(dir.path().join("src/one.rs"), "fn one() {}\n").unwrap();
    fs::write(dir.path().join("src/two.rs"), "fn two() {}\n").unwrap();
    fs::create_dir_all(dir.path().join(".tsift")).unwrap();
    fs::write(
        dir.path().join(".tsift/config.toml"),
        "[summarize]\napi_key_env = \"TSIFT_TEST_NONEXISTENT_KEY\"\n",
    )
    .unwrap();
    let empty_path = tempfile::tempdir().unwrap();

    let output = tsift_bin()
        .env("PATH", empty_path.path())
        .env_remove("TSIFT_TEST_NONEXISTENT_KEY")
        .env_remove("CLAUDE_CODE_USE_BEDROCK")
        .env_remove("CLAUDE_CODE_USE_VERTEX")
        .env_remove("CLAUDE_CODE_USE_FOUNDRY")
        .args([
            "summarize",
            "--extract",
            "src",
            "--path",
            dir.path().to_str().unwrap(),
        ])
        .output()
        .unwrap();

    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_eq!(stderr.matches("no LLM credentials found").count(), 1);
    assert!(stderr.contains("TSIFT_TEST_NONEXISTENT_KEY"));
    assert!(!stderr.contains("src/one.rs"), "stderr was: {stderr}");
    assert!(!stderr.contains("src/two.rs"), "stderr was: {stderr}");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(!stdout.contains("Extraction complete"));
}

#[cfg(unix)]
#[test]
fn summarize_extract_uses_claude_cli_for_bedrock_without_an_api_key() {
    let dir = tempfile::tempdir().unwrap();
    fs::create_dir_all(dir.path().join("src")).unwrap();
    fs::write(
        dir.path().join("src/main.rs"),
        "fn bedrock_summary_target() {}\n",
    )
    .unwrap();
    let bin_dir = dir.path().join("bin");
    fs::create_dir_all(&bin_dir).unwrap();
    let claude = bin_dir.join("claude");
    fs::write(
        &claude,
        r#"#!/bin/sh
printf '%s\n' "$@" > "$TSIFT_TEST_CLAUDE_ARGS"
prompt=$(/bin/cat)
printf '%s' "$prompt" > "$TSIFT_TEST_CLAUDE_PROMPT"
printf '%s\n' '{"result":"{\"summary\":\"bedrock works\",\"entities\":[],\"relationships\":[],\"concept_labels\":[]}","usage":{"input_tokens":12,"cache_creation_input_tokens":3,"cache_read_input_tokens":40,"output_tokens":7}}'
"#,
    )
    .unwrap();
    let mut permissions = fs::metadata(&claude).unwrap().permissions();
    permissions.set_mode(0o755);
    fs::set_permissions(&claude, permissions).unwrap();
    let args_path = dir.path().join("claude-args.txt");
    let prompt_path = dir.path().join("claude-prompt.txt");

    let output = tsift_bin()
        .env("PATH", &bin_dir)
        .env("CLAUDE_CODE_USE_BEDROCK", "1")
        .env_remove("ANTHROPIC_API_KEY")
        .env("TSIFT_TEST_CLAUDE_ARGS", &args_path)
        .env("TSIFT_TEST_CLAUDE_PROMPT", &prompt_path)
        .args([
            "summarize",
            "--extract",
            "src",
            "--path",
            dir.path().to_str().unwrap(),
            "--compact",
        ])
        .output()
        .unwrap();

    assert!(
        output.status.success(),
        "summarize stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("files:1"), "stdout was: {stdout}");
    assert!(stdout.contains("errors:0"), "stdout was: {stdout}");
    assert!(stdout.contains("tokens_in:55"), "stdout was: {stdout}");
    assert!(stdout.contains("tokens_out:7"), "stdout was: {stdout}");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("extracting 1/1: src/main.rs"),
        "stderr was: {stderr}"
    );
    let args = fs::read_to_string(args_path).unwrap();
    assert!(args.lines().any(|arg| arg == "-p"), "args were: {args}");
    assert!(
        args.lines().any(|arg| arg == "--model"),
        "args were: {args}"
    );
    assert!(
        args.lines().any(|arg| arg == "--safe-mode"),
        "args were: {args}"
    );
    assert!(
        args.lines().any(|arg| arg == "--output-format"),
        "args were: {args}"
    );
    assert!(args.lines().any(|arg| arg == "json"), "args were: {args}");
    let prompt = fs::read_to_string(prompt_path).unwrap();
    assert!(prompt.contains("bedrock_summary_target"));
}

#[cfg(unix)]
#[test]
fn summarize_extract_rejects_an_unauthenticated_claude_cli_before_walking_files() {
    let dir = tempfile::tempdir().unwrap();
    fs::create_dir_all(dir.path().join("src")).unwrap();
    fs::write(dir.path().join("src/one.rs"), "fn one() {}\n").unwrap();
    fs::write(dir.path().join("src/two.rs"), "fn two() {}\n").unwrap();
    let bin_dir = dir.path().join("bin");
    fs::create_dir_all(&bin_dir).unwrap();
    let claude = bin_dir.join("claude");
    fs::write(
        &claude,
        r#"#!/bin/sh
if [ "$1" = "auth" ]; then
  printf '%s\n' 'not logged in' >&2
  exit 1
fi
printf '%s' 'extraction unexpectedly started' > "$TSIFT_TEST_CLAUDE_EXTRACTED"
"#,
    )
    .unwrap();
    let mut permissions = fs::metadata(&claude).unwrap().permissions();
    permissions.set_mode(0o755);
    fs::set_permissions(&claude, permissions).unwrap();
    let extraction_marker = dir.path().join("extraction-started.txt");

    let output = tsift_bin()
        .env("PATH", &bin_dir)
        .env("CLAUDE_CODE_USE_BEDROCK", "1")
        .env_remove("ANTHROPIC_API_KEY")
        .env("TSIFT_TEST_CLAUDE_EXTRACTED", &extraction_marker)
        .args([
            "summarize",
            "--extract",
            "src",
            "--path",
            dir.path().to_str().unwrap(),
        ])
        .output()
        .unwrap();

    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_eq!(stderr.matches("not a usable extraction backend").count(), 1);
    assert!(stderr.contains("claude auth login"), "stderr was: {stderr}");
    assert!(!stderr.contains("src/one.rs"), "stderr was: {stderr}");
    assert!(!stderr.contains("src/two.rs"), "stderr was: {stderr}");
    assert!(!extraction_marker.exists());
}

#[test]
fn summarize_diff_extract_includes_untracked_files() {
    let dir = tempfile::tempdir().unwrap();
    fs::write(dir.path().join("README.md"), "# repo\n").unwrap();
    init_git_repo(dir.path());

    fs::create_dir_all(dir.path().join("src")).unwrap();
    fs::write(dir.path().join("src/new.rs"), "fn alpha_helper() {}\n").unwrap();

    let mut command = tsift_bin();
    let output = mock_anthropic_extraction(&mut command)
        .args([
            "summarize",
            "--extract",
            "src",
            "--diff",
            "--path",
            dir.path().to_str().unwrap(),
            "--compact",
        ])
        .output()
        .unwrap();

    assert!(
        output.status.success(),
        "summarize stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        !stdout.contains("No files to extract."),
        "stdout was: {stdout}"
    );
    assert!(stdout.contains("files:1"), "stdout was: {stdout}");
    assert!(stdout.contains("errors:0"), "stdout was: {stdout}");
}

#[test]
fn summarize_diff_extract_treats_unborn_head_as_untracked_only() {
    let dir = tempfile::tempdir().unwrap();
    let status = Command::new("git")
        .args(["init"])
        .current_dir(dir.path())
        .status()
        .unwrap();
    assert!(status.success(), "git init failed");

    fs::create_dir_all(dir.path().join("src")).unwrap();
    fs::write(dir.path().join("src/new.rs"), "fn alpha_helper() {}\n").unwrap();

    let mut command = tsift_bin();
    let output = mock_anthropic_extraction(&mut command)
        .args([
            "summarize",
            "--extract",
            "src",
            "--diff",
            "--path",
            dir.path().to_str().unwrap(),
            "--compact",
        ])
        .output()
        .unwrap();

    assert!(
        output.status.success(),
        "summarize stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        !stdout.contains("No files to extract."),
        "stdout was: {stdout}"
    );
    assert!(stdout.contains("files:1"), "stdout was: {stdout}");
    assert!(stdout.contains("errors:0"), "stdout was: {stdout}");
}

#[test]
fn summarize_diff_extract_normalizes_relative_scope_before_filtering() {
    let dir = tempfile::tempdir().unwrap();
    fs::create_dir_all(dir.path().join("src")).unwrap();
    fs::write(dir.path().join("src/lib.rs"), "fn alpha_helper() {}\n").unwrap();
    fs::write(dir.path().join("README.md"), "# repo\n").unwrap();
    init_git_repo(dir.path());

    fs::write(
        dir.path().join("src/lib.rs"),
        "fn alpha_helper() {}\nfn beta_helper() {}\n",
    )
    .unwrap();

    let mut command = tsift_bin();
    let output = mock_anthropic_extraction(&mut command)
        .args([
            "summarize",
            "--extract",
            "src/../src",
            "--diff",
            "--path",
            dir.path().to_str().unwrap(),
            "--compact",
        ])
        .output()
        .unwrap();

    assert!(
        output.status.success(),
        "summarize stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        !stdout.contains("No files to extract."),
        "stdout was: {stdout}"
    );
    assert!(stdout.contains("files:1"), "stdout was: {stdout}");
    assert!(stdout.contains("errors:0"), "stdout was: {stdout}");
}

#[test]
fn summarize_extract_uses_matching_scoped_index_prompt_context() {
    let dir = tempfile::tempdir().unwrap();
    fs::write(
        dir.path().join(".gitmodules"),
        r#"[submodule "src/alpha"]
	path = src/alpha
	url = https://example.com/alpha
[submodule "src/beta"]
	path = src/beta
	url = https://example.com/beta
"#,
    )
    .unwrap();
    fs::create_dir_all(dir.path().join("src/alpha/src")).unwrap();
    fs::create_dir_all(dir.path().join("src/beta/src")).unwrap();
    fs::write(
        dir.path().join("src/alpha/src/lib.rs"),
        "fn alpha_helper() {}\nfn alpha_entry() { alpha_helper(); }\n",
    )
    .unwrap();
    fs::write(
        dir.path().join("src/beta/src/lib.rs"),
        "fn beta_helper() {}\nfn beta_entry() { beta_helper(); }\n",
    )
    .unwrap();

    let output = tsift_bin()
        .args(["index", "--workspace", dir.path().to_str().unwrap()])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "index stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let prompt_path = dir.path().join("captured-prompt.txt");
    let output = tsift_bin()
        .current_dir(dir.path())
        .env("ANTHROPIC_API_KEY", "test-key")
        .env(
            "TSIFT_TEST_ANTHROPIC_RESPONSE_JSON",
            r#"{"summary":"ok","entities":[],"relationships":[],"concept_labels":[]}"#,
        )
        .env("TSIFT_TEST_ANTHROPIC_CAPTURE_PROMPT", &prompt_path)
        .args([
            "summarize",
            "--extract",
            "src/beta/src/lib.rs",
            "--path",
            dir.path().to_str().unwrap(),
            "--compact",
        ])
        .output()
        .unwrap();

    assert!(
        output.status.success(),
        "summarize stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let prompt = fs::read_to_string(&prompt_path).unwrap();
    assert!(
        prompt.contains("- beta_helper (function)"),
        "prompt was: {prompt}"
    );
    assert!(
        !prompt.contains("- alpha_helper (function)"),
        "prompt was: {prompt}"
    );
}

#[cfg(unix)]
#[test]
fn summarize_symbol_query_accepts_read_only_cache_permissions() {
    let dir = tempfile::tempdir().unwrap();
    create_summary_cache(dir.path());

    let db_path = dir.path().join(".tsift/summaries.db");
    let original_mode = fs::metadata(&db_path).unwrap().permissions().mode();
    let mut read_only = fs::metadata(&db_path).unwrap().permissions();
    read_only.set_mode(0o444);
    fs::set_permissions(&db_path, read_only).unwrap();

    let output = tsift_bin()
        .args([
            "summarize",
            "alpha_helper",
            "--path",
            dir.path().to_str().unwrap(),
            "--json",
        ])
        .output()
        .unwrap();

    let mut restored = fs::metadata(&db_path).unwrap().permissions();
    restored.set_mode(original_mode);
    fs::set_permissions(&db_path, restored).unwrap();

    assert!(
        output.status.success(),
        "summarize stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let json: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    let summaries = json.as_array().unwrap();
    assert!(
        summaries
            .iter()
            .any(|summary| summary["symbol_name"] == "alpha_helper"),
        "stdout was: {}",
        String::from_utf8_lossy(&output.stdout)
    );
}

#[cfg(unix)]
#[test]
fn summarize_file_query_accepts_absolute_symlinked_checkout_path() {
    use std::os::unix::fs::symlink;

    let dir = tempfile::tempdir().unwrap();
    fs::create_dir_all(dir.path().join("src")).unwrap();
    fs::write(dir.path().join("src/lib.rs"), "fn alpha_helper() {}\n").unwrap();
    create_summary_cache(dir.path());

    let link_parent = tempfile::tempdir().unwrap();
    let link_root = link_parent.path().join("repo-link");
    symlink(dir.path(), &link_root).unwrap();
    let symlinked_file = link_root.join("src/lib.rs");

    let output = tsift_bin()
        .args([
            "summarize",
            "--file",
            symlinked_file.to_str().unwrap(),
            "--path",
            link_root.to_str().unwrap(),
            "--json",
        ])
        .output()
        .unwrap();

    assert!(
        output.status.success(),
        "summarize stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let json: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(json[0]["symbol_name"], "alpha_helper");
    assert_eq!(json[0]["file_path"], "src/lib.rs");
}

#[test]
fn summarize_symbol_query_uses_ancestor_project_root_for_nested_paths() {
    let dir = tempfile::tempdir().unwrap();
    create_summary_cache(dir.path());
    fs::create_dir_all(dir.path().join("src/nested")).unwrap();

    let nested = dir.path().join("src/nested");
    let output = tsift_bin()
        .args([
            "summarize",
            "alpha_helper",
            "--path",
            nested.to_str().unwrap(),
            "--json",
        ])
        .output()
        .unwrap();

    assert!(
        output.status.success(),
        "summarize stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let json: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    let summaries = json.as_array().unwrap();
    assert!(
        summaries
            .iter()
            .any(|summary| summary["symbol_name"] == "alpha_helper"),
        "stdout was: {}",
        String::from_utf8_lossy(&output.stdout)
    );
    assert!(!nested.join(".tsift/summaries.db").exists());
}

#[test]
fn summarize_file_query_normalizes_equivalent_paths() {
    let dir = tempfile::tempdir().unwrap();
    create_summary_cache(dir.path());
    fs::create_dir_all(dir.path().join("src/nested")).unwrap();

    let root_relative = tsift_bin()
        .args([
            "summarize",
            "--file",
            "./src/lib.rs",
            "--path",
            dir.path().to_str().unwrap(),
            "--json",
        ])
        .output()
        .unwrap();
    assert!(
        root_relative.status.success(),
        "summarize stderr: {}",
        String::from_utf8_lossy(&root_relative.stderr)
    );
    let root_relative_json: serde_json::Value =
        serde_json::from_slice(&root_relative.stdout).unwrap();
    let root_relative_summaries = root_relative_json.as_array().unwrap();
    assert!(
        root_relative_summaries
            .iter()
            .any(|summary| summary["symbol_name"] == "alpha_helper"),
        "stdout was: {}",
        String::from_utf8_lossy(&root_relative.stdout)
    );

    let nested = dir.path().join("src/nested");
    let nested_relative = tsift_bin()
        .args([
            "summarize",
            "--file",
            "../lib.rs",
            "--path",
            nested.to_str().unwrap(),
            "--json",
        ])
        .output()
        .unwrap();
    assert!(
        nested_relative.status.success(),
        "summarize stderr: {}",
        String::from_utf8_lossy(&nested_relative.stderr)
    );
    let nested_relative_json: serde_json::Value =
        serde_json::from_slice(&nested_relative.stdout).unwrap();
    let nested_relative_summaries = nested_relative_json.as_array().unwrap();
    assert!(
        nested_relative_summaries
            .iter()
            .any(|summary| summary["symbol_name"] == "alpha_helper"),
        "stdout was: {}",
        String::from_utf8_lossy(&nested_relative.stdout)
    );
}

#[test]
fn summarize_file_query_reads_legacy_windows_separator_cache_rows() {
    let dir = tempfile::tempdir().unwrap();
    create_summary_cache(dir.path());

    let conn = Connection::open(dir.path().join(".tsift/summaries.db")).unwrap();
    conn.execute("DELETE FROM summaries", []).unwrap();
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
            "alpha_helper",
            r"src\lib.rs",
            "hash1",
            "cached summary",
            Option::<String>::None,
            Option::<String>::None,
            Option::<String>::None,
            "1700000000",
            "claude-haiku-4-5-20251001",
            100_i64,
            40_i64
        ],
    )
    .unwrap();

    let output = tsift_bin()
        .args([
            "summarize",
            "--file",
            "./src/lib.rs",
            "--path",
            dir.path().to_str().unwrap(),
            "--json",
        ])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "summarize stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let json: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(json[0]["symbol_name"], "alpha_helper");
    assert_eq!(json[0]["file_path"], "src/lib.rs");
}

#[test]
fn index_reports_lock_diagnostics_when_rollback_journal_blocks_writer() {
    let dir = tempfile::tempdir().unwrap();
    fs::write(dir.path().join("main.rs"), "fn main() {}").unwrap();

    let status = tsift_bin()
        .args(["index", dir.path().to_str().unwrap()])
        .status()
        .unwrap();
    assert!(status.success());

    let _lock = hold_rollback_journal_lock(&dir.path().join(".tsift/index.db"));

    let output = tsift_bin()
        .args(["index", dir.path().to_str().unwrap()])
        .output()
        .unwrap();

    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("lock diagnostics:"));
    assert!(stderr.contains("journal: present"));
    assert!(stderr.contains("run: tsift index"));
    assert!(stderr.contains("next: inspect the host for a wedged rollback-journal writer"));
}

#[test]
fn search_timeout_kills_worker_process() {
    let dir = tempfile::tempdir().unwrap();
    fs::write(dir.path().join("main.rs"), "fn main() {}").unwrap();
    let pid_file = dir.path().join("worker.pid");

    let started = Instant::now();
    let output = tsift_bin()
        .env("TSIFT_TEST_SEARCH_WORKER_SLEEP_MS", "5000")
        .env("TSIFT_TEST_SEARCH_WORKER_PID_FILE", &pid_file)
        .args([
            "search",
            "--timeout",
            "1",
            "--path",
            dir.path().to_str().unwrap(),
            "main",
        ])
        .output()
        .unwrap();

    assert!(!output.status.success(), "expected timeout failure");
    assert!(
        started.elapsed() < Duration::from_secs(4),
        "timeout should return promptly"
    );

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("timed out after 1s"));
    assert!(stderr.contains("search root looks fresh"));
    assert!(!stderr.contains("index may be stale"));

    let pid = fs::read_to_string(&pid_file)
        .unwrap()
        .trim()
        .parse::<u32>()
        .unwrap();
    assert!(
        wait_for_process_exit(pid, Duration::from_secs(2)),
        "timed-out worker process {pid} should be gone"
    );
}

#[test]
fn search_timeout_reports_reindex_when_index_turns_stale_during_worker_run() {
    let dir = tempfile::tempdir().unwrap();
    let source = dir.path().join("main.rs");
    fs::write(&source, "fn main() {}\n").unwrap();

    let status = tsift_bin()
        .args(["index", dir.path().to_str().unwrap()])
        .status()
        .unwrap();
    assert!(status.success());

    let source_for_writer = source.clone();
    let writer = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(100));
        fs::write(
            &source_for_writer,
            "fn helper() {}\nfn main() { helper(); }\n",
        )
        .unwrap();
    });

    let output = tsift_bin()
        .env("TSIFT_TEST_SEARCH_WORKER_SLEEP_MS", "5000")
        .args([
            "search",
            "--timeout",
            "1",
            "--path",
            dir.path().to_str().unwrap(),
            "main",
        ])
        .output()
        .unwrap();
    writer.join().unwrap();

    assert!(!output.status.success(), "expected timeout failure");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("timed out after 1s"));
    assert!(stderr.contains("index is stale"));
    assert!(stderr.contains("Run `tsift index"));
}

#[test]
fn search_timeout_reports_reindex_when_index_disappears_during_worker_run() {
    let dir = tempfile::tempdir().unwrap();
    let source = dir.path().join("main.rs");
    let index_path = dir.path().join(".tsift/index.db");
    fs::write(&source, "fn main() {}\n").unwrap();

    let status = tsift_bin()
        .args(["index", dir.path().to_str().unwrap()])
        .status()
        .unwrap();
    assert!(status.success());

    let index_path_for_remover = index_path.clone();
    let remover = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(100));
        fs::remove_file(&index_path_for_remover).unwrap();
    });

    let output = tsift_bin()
        .env("TSIFT_TEST_SEARCH_WORKER_SLEEP_MS", "5000")
        .args([
            "search",
            "--timeout",
            "1",
            "--path",
            dir.path().to_str().unwrap(),
            "main",
        ])
        .output()
        .unwrap();
    remover.join().unwrap();

    assert!(!output.status.success(), "expected timeout failure");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("timed out after 1s"));
    assert!(stderr.contains("index is missing"));
    assert!(stderr.contains("Run `tsift index"));
    assert!(!stderr.contains("search root looks fresh"));
}

#[test]
fn search_timeout_zero_keeps_search_in_process() {
    let dir = tempfile::tempdir().unwrap();
    fs::write(dir.path().join("main.rs"), "fn main() {}").unwrap();
    let pid_file = dir.path().join("worker.pid");

    let output = tsift_bin()
        .env("TSIFT_TEST_SEARCH_WORKER_SLEEP_MS", "5000")
        .env("TSIFT_TEST_SEARCH_WORKER_PID_FILE", &pid_file)
        .args([
            "search",
            "--timeout",
            "0",
            "--path",
            dir.path().to_str().unwrap(),
            "main",
        ])
        .output()
        .unwrap();

    assert!(
        output.status.success(),
        "timeout=0 should bypass worker timeout path"
    );
    assert!(
        !pid_file.exists(),
        "timeout=0 should not spawn the hidden search worker"
    );
    // #015t Phase 4: lexical search now defaults to the FTS5 `index.db` path, so
    // autoindex builds index.db in-process (the legacy `search-cache` token index
    // is no longer written on the default path).
    assert!(
        dir.path().join(".tsift/index.db").exists(),
        "timeout=0 in-process search should autoindex the root index.db"
    );
}

#[test]
fn diff_digest_reports_changed_symbols_and_call_edges() {
    let dir = tempfile::tempdir().unwrap();
    let source = dir.path().join("main.rs");
    fs::write(&source, "fn old_helper() {}\nfn main() { old_helper(); }\n").unwrap();
    init_git_repo(dir.path());

    fs::write(&source, "fn new_helper() {}\nfn main() { new_helper(); }\n").unwrap();

    let output = tsift_bin()
        .args(["diff-digest", "--json", dir.path().to_str().unwrap()])
        .output()
        .unwrap();

    assert!(output.status.success(), "diff-digest should succeed");
    let json: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(json["files_changed"], 1);
    assert_eq!(json["files"][0]["path"], "main.rs");
    assert_eq!(json["files"][0]["status"], "modified");
    assert!(
        json["files"][0]["touched_symbols"]
            .as_array()
            .unwrap()
            .iter()
            .any(|symbol| symbol == "new_helper")
    );
    assert!(
        json["files"][0]["removed_call_edges"]
            .as_array()
            .unwrap()
            .iter()
            .any(|edge| edge == "main -> old_helper")
    );
    assert!(
        json["files"][0]["added_call_edges"]
            .as_array()
            .unwrap()
            .iter()
            .any(|edge| edge == "main -> new_helper")
    );
}

#[test]
fn diff_digest_cached_reads_staged_snapshot() {
    let dir = tempfile::tempdir().unwrap();
    let source = dir.path().join("main.rs");
    fs::write(&source, "fn old_helper() {}\nfn main() { old_helper(); }\n").unwrap();
    init_git_repo(dir.path());

    fs::write(
        &source,
        "fn staged_helper() {}\nfn main() { staged_helper(); }\n",
    )
    .unwrap();
    let status = Command::new("git")
        .args(["add", "main.rs"])
        .current_dir(dir.path())
        .status()
        .unwrap();
    assert!(status.success(), "git add failed");

    fs::write(
        &source,
        "fn unstaged_helper() {}\nfn main() { unstaged_helper(); }\n",
    )
    .unwrap();

    let output = tsift_bin()
        .args([
            "diff-digest",
            "--cached",
            "--json",
            dir.path().to_str().unwrap(),
        ])
        .output()
        .unwrap();

    assert!(output.status.success(), "cached diff-digest should succeed");
    let json: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(json["mode"], "cached");
    assert!(
        json["files"][0]["touched_symbols"]
            .as_array()
            .unwrap()
            .iter()
            .any(|symbol| symbol == "staged_helper")
    );
    assert!(
        !json["files"][0]["touched_symbols"]
            .as_array()
            .unwrap()
            .iter()
            .any(|symbol| symbol == "unstaged_helper")
    );
}

#[test]
fn diff_digest_revision_reads_commit_snapshot() {
    let dir = tempfile::tempdir().unwrap();
    let source = dir.path().join("main.rs");
    fs::write(&source, "fn old_helper() {}\nfn main() { old_helper(); }\n").unwrap();
    init_git_repo(dir.path());

    fs::write(
        &source,
        "fn committed_helper() {}\nfn main() { committed_helper(); }\n",
    )
    .unwrap();
    let status = Command::new("git")
        .args(["add", "main.rs"])
        .current_dir(dir.path())
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
            "second",
        ])
        .current_dir(dir.path())
        .status()
        .unwrap();
    assert!(status.success(), "git commit failed");

    fs::write(
        &source,
        "fn working_tree_only() {}\nfn main() { working_tree_only(); }\n",
    )
    .unwrap();

    let output = tsift_bin()
        .args([
            "diff-digest",
            "--revision",
            "HEAD",
            "--json",
            dir.path().to_str().unwrap(),
        ])
        .output()
        .unwrap();

    assert!(
        output.status.success(),
        "revision diff-digest should succeed"
    );
    let json: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(json["mode"], "revision");
    assert!(json["revision"].as_str().is_some());
    assert!(
        json["files"][0]["touched_symbols"]
            .as_array()
            .unwrap()
            .iter()
            .any(|symbol| symbol == "committed_helper")
    );
    assert!(
        !json["files"][0]["touched_symbols"]
            .as_array()
            .unwrap()
            .iter()
            .any(|symbol| symbol == "working_tree_only")
    );
}

#[test]
fn test_digest_reads_cargo_output_from_stdin() {
    let dir = tempfile::tempdir().unwrap();
    fs::create_dir_all(dir.path().join("src")).unwrap();
    fs::write(dir.path().join("src/lib.rs"), "fn helper() {}\n").unwrap();

    let input = "\
running 2 tests
---- tests::alpha stdout ----
thread 'tests::alpha' panicked at src/lib.rs:7:9:
assertion `left == right` failed

test result: FAILED. 0 passed; 1 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.00s
";

    let mut child = tsift_bin()
        .args([
            "test-digest",
            "--json",
            "--path",
            dir.path().to_str().unwrap(),
        ])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    {
        use std::io::Write;
        child
            .stdin
            .as_mut()
            .unwrap()
            .write_all(input.as_bytes())
            .unwrap();
    }
    let output = child.wait_with_output().unwrap();

    assert!(output.status.success(), "test-digest should succeed");
    let json: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(json["runner"], "cargo");
    assert_eq!(json["failures"], 1);
    assert_eq!(json["grouped_failures"], 1);
    assert_eq!(json["counts"]["failed"], 1);
    assert_eq!(json["failure_groups"][0]["path"], "src/lib.rs");
    assert_eq!(json["failure_groups"][0]["line"], 7);
}

#[test]
fn log_digest_reads_verbose_output_from_stdin() {
    let dir = tempfile::tempdir().unwrap();
    fs::create_dir_all(dir.path().join("src")).unwrap();
    fs::write(dir.path().join("src/lib.rs"), "fn run_sync() {}\n").unwrap();

    let input = "\
error: run_sync failed at src/lib.rs:1:1
error: run_sync failed at src/lib.rs:1:1
warning: retrying run_sync
warning: retrying run_sync
0: my_crate::run_sync
at src/lib.rs:1:1
";

    let mut child = tsift_bin()
        .args([
            "log-digest",
            "--json",
            "--path",
            dir.path().to_str().unwrap(),
        ])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    {
        use std::io::Write;
        child
            .stdin
            .as_mut()
            .unwrap()
            .write_all(input.as_bytes())
            .unwrap();
    }
    let output = child.wait_with_output().unwrap();

    assert!(output.status.success(), "log-digest should succeed");
    let json: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(json["signal_groups"], 2);
    assert_eq!(json["repeated_line_groups"], 2);
    assert_eq!(json["file_refs"][0]["path"], "src/lib.rs");
    assert!(
        json["symbol_refs"]
            .as_array()
            .unwrap()
            .iter()
            .any(|symbol| symbol["symbol"] == "run_sync")
    );
}

#[test]
fn log_digest_reads_agent_doc_structured_runtime_fields() {
    let dir = tempfile::tempdir().unwrap();
    fs::create_dir_all(dir.path().join("tasks/software")).unwrap();
    fs::write(dir.path().join("tasks/software/tsift.md"), "# tsift\n").unwrap();

    let input = "\
[1778646072] route_dispatch_start_proven file=tasks/software/tsift.md pane=%31 harness=codex proof=consumed timeout_secs=10
[1778646078] document_cycle phase=committed cycle=cycle-1778644920810 event=commit_success session=tsift-v0.1 pane=%31
";

    let mut child = tsift_bin()
        .args([
            "log-digest",
            "--json",
            "--path",
            dir.path().to_str().unwrap(),
        ])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    {
        use std::io::Write;
        child
            .stdin
            .as_mut()
            .unwrap()
            .write_all(input.as_bytes())
            .unwrap();
    }
    let output = child.wait_with_output().unwrap();

    assert!(output.status.success(), "log-digest should succeed");
    let json: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(json["file_ref_groups"], 1);
    assert_eq!(json["file_refs"][0]["path"], "tasks/software/tsift.md");
    assert!(json["file_refs"][0]["line"].is_null());
    assert!(
        json["symbol_refs"]
            .as_array()
            .unwrap()
            .iter()
            .any(|symbol| symbol["symbol"] == "event:commit_success")
    );
    assert!(
        json["symbol_refs"]
            .as_array()
            .unwrap()
            .iter()
            .any(|symbol| symbol["symbol"] == "pane:%31")
    );
    assert!(
        !json["warnings"]
            .as_array()
            .unwrap()
            .iter()
            .any(|warning| warning == "no file anchors detected")
    );
}

#[test]
fn log_digest_classifies_agent_doc_runtime_events_as_signals() {
    let dir = tempfile::tempdir().unwrap();
    fs::create_dir_all(dir.path().join("tasks/software")).unwrap();
    fs::write(dir.path().join("tasks/software/tsift.md"), "# tsift\n").unwrap();

    let input = "\
[1776528398] claude_start mode=fresh_restart restart_count=1 file=tasks/software/tsift.md
[1776528446] auto_trigger_timeout harness=codex reason=no_prompt_after_30s
[1776528450] ctrl_d_restart_fresh restart_count=2 file=tasks/software/tsift.md
[1776528451] user_quit_after_ctrl_d
[1776528452] supervisor_exit reason=user_quit_after_eof restart_count=0
[1776528532] claude_exit code=1 restart_count=0
[1777603403] document_cycle phase=committed cycle=cycle-1 event=commit_already_current
[1777603404] document_cycle phase=committed cycle=cycle-2 event=commit_already_current
";

    let mut child = tsift_bin()
        .args([
            "log-digest",
            "--json",
            "--path",
            dir.path().to_str().unwrap(),
        ])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    {
        use std::io::Write;
        child
            .stdin
            .as_mut()
            .unwrap()
            .write_all(input.as_bytes())
            .unwrap();
    }
    let output = child.wait_with_output().unwrap();

    assert!(output.status.success(), "log-digest should succeed");
    let json: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(json["signal_groups"], 6);
    let signals = json["signals"].as_array().unwrap();
    assert!(signals.iter().any(|signal| {
        signal["severity"] == "error" && signal["message"] == "agent-doc exit: claude_exit code=1"
    }));
    assert!(
        signals
            .iter()
            .any(|signal| signal["message"] == "agent-doc timeout: auto_trigger_timeout")
    );
    assert!(signals.iter().any(|signal| {
        signal["message"] == "agent-doc restart churn: fresh_restart" && signal["occurrences"] == 2
    }));
    assert!(
        signals
            .iter()
            .any(|signal| { signal["message"] == "agent-doc restart churn: auto_trigger_timeout" })
    );
    assert!(
        signals
            .iter()
            .any(|signal| { signal["message"] == "agent-doc restart churn: ctrl_d_restart_loop" })
    );
    assert!(
        !signals
            .iter()
            .any(|signal| { signal["message"] == "agent-doc restart churn: quit_after_eof" })
    );
    assert!(signals.iter().any(|signal| {
        signal["message"] == "agent-doc closeout churn: commit_already_current"
            && signal["occurrences"] == 2
    }));
}

#[test]
fn metric_digest_reads_run_history_from_stdin() {
    let input = r#"{
  "runs": [
    {"label": "bootstrap-20260503", "metrics": {"session_mae": 1.11, "composite_score": 67.5, "cost_usd": 4.20}},
    {"label": "bootstrap-20260504", "metrics": {"session_mae": 1.07, "composite_score": 69.4, "cost_usd": 4.60}}
  ]
}"#;

    let mut child = tsift_bin()
        .args(["metric-digest", "--json"])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    {
        use std::io::Write;
        child
            .stdin
            .as_mut()
            .unwrap()
            .write_all(input.as_bytes())
            .unwrap();
    }
    let output = child.wait_with_output().unwrap();

    assert!(output.status.success(), "metric-digest should succeed");
    let json: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(json["runs_loaded"], 2);
    assert_eq!(json["current_run"]["label"], "bootstrap-20260504");
    assert_eq!(json["previous_run"]["label"], "bootstrap-20260503");
    assert_eq!(json["shared_metrics"], 3);
    assert!(
        json["top_improvements"]
            .as_array()
            .unwrap()
            .iter()
            .any(|delta| delta["metric"] == "session_mae")
    );
    assert!(
        json["news_table_markdown"]
            .as_str()
            .unwrap()
            .contains("| run |")
    );
}

#[test]
fn metric_digest_reports_community_search_gate_fixture() {
    let fixture =
        Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures/community-search-gate-history.json");
    assert!(
        fixture.exists(),
        "community search gate fixture should exist at {}",
        fixture.display()
    );

    let output = tsift_bin()
        .args([
            "metric-digest",
            "--input",
            fixture.to_str().unwrap(),
            "--json",
        ])
        .output()
        .unwrap();

    assert!(
        output.status.success(),
        "metric-digest should succeed for the community search gate fixture"
    );
    let json: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    let gate = &json["community_search_gate"];
    assert_eq!(gate["decision"], "pass");
    assert_eq!(gate["workloads"].as_array().unwrap().len(), 2);
    assert_eq!(gate["min_handle_coverage_pct"], 95.0);
    assert_eq!(gate["min_duplicate_name_precision"], 0.99);
    assert!(
        gate["required_metrics"]
            .as_array()
            .unwrap()
            .iter()
            .any(|metric| metric == "handle_coverage_pct")
    );
    assert!(
        gate["required_metrics"]
            .as_array()
            .unwrap()
            .iter()
            .any(|metric| metric == "duplicate_name_precision")
    );
    assert!(
        gate["required_metrics"]
            .as_array()
            .unwrap()
            .iter()
            .any(|metric| metric == "top_community_stability")
    );
    assert!(
        gate["workloads"]
            .as_array()
            .unwrap()
            .iter()
            .any(|workload| { workload["workload"] == "real" && workload["status"] == "pass" })
    );
    assert!(
        gate["workloads"]
            .as_array()
            .unwrap()
            .iter()
            .any(|workload| {
                workload["workload"] == "synthetic_multi_module" && workload["status"] == "pass"
            })
    );
    assert!(
        json["metric_deltas"]
            .as_array()
            .unwrap()
            .iter()
            .any(|delta| {
                delta["metric"] == "communities.real.duration_micros"
                    && delta["trend"] == "regressed"
            })
    );
}

#[test]
fn metric_digest_reports_memgraphrag_performance_gate_fixture() {
    let fixture =
        Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures/memgraphrag-performance-history.json");
    assert!(
        fixture.exists(),
        "MemGraphRAG performance fixture should exist at {}",
        fixture.display()
    );

    let output = tsift_bin()
        .args([
            "metric-digest",
            "--input",
            fixture.to_str().unwrap(),
            "--json",
        ])
        .output()
        .unwrap();

    assert!(
        output.status.success(),
        "metric-digest should succeed for the MemGraphRAG performance fixture: stderr={}",
        String::from_utf8_lossy(&output.stderr)
    );
    let json: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    let gate = &json["memgraphrag_performance_gate"];
    assert_eq!(gate["decision"], "pass");
    assert_eq!(
        gate["baseline_fixture"],
        "fixtures/memgraphrag-performance-history.json"
    );
    assert_eq!(gate["workloads"].as_array().unwrap().len(), 4);
    assert_eq!(gate["max_duration_regression_percent"], 25.0);
    assert!(
        gate["required_metrics"]
            .as_array()
            .unwrap()
            .iter()
            .any(|metric| metric == "duration_micros")
    );
    for workload in [
        "memory_query",
        "memory_project_graph",
        "graph_db_related",
        "semantic_seeded_neighborhood",
    ] {
        assert!(
            gate["workloads"].as_array().unwrap().iter().any(|row| {
                row["workload"] == workload
                    && row["status"] == "pass"
                    && row["duration_micros"].as_f64().unwrap() > 0.0
            }),
            "MemGraphRAG gate should pass workload {workload}: {gate}"
        );
    }
    assert!(
        json["metric_deltas"]
            .as_array()
            .unwrap()
            .iter()
            .any(|delta| {
                delta["metric"] == "memgraphrag.memory_project_graph.duration_micros"
                    && delta["trend"] == "regressed"
            })
    );
}

#[test]
fn dci_benchmark_summarizes_recorded_strategy_fixture() {
    let fixture = Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures/dci-search-benchmark.json");
    assert!(
        fixture.exists(),
        "DCI benchmark fixture should exist at {}",
        fixture.display()
    );

    let output = tsift_bin()
        .args([
            "dci-benchmark",
            "--fixture",
            fixture.to_str().unwrap(),
            "--json",
        ])
        .output()
        .unwrap();

    assert!(
        output.status.success(),
        "dci-benchmark should pass: stderr={}",
        String::from_utf8_lossy(&output.stderr)
    );
    let json: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(json["tasks_loaded"], 3);
    assert_eq!(json["strategies_compared"], 3);
    assert_eq!(
        json["strategy_summaries"][0]["strategy"],
        "exact_chained_rg"
    );
    assert_eq!(json["strategy_summaries"][0]["localized"], 3);
    assert!(json["task_rows"].as_array().unwrap().iter().all(|row| {
        row["best_localization"]
            .as_array()
            .unwrap()
            .iter()
            .any(|strategy| strategy == "exact_chained_rg")
    }));
    assert!(
        json.get("warnings")
            .is_none_or(|value| value.as_array().unwrap().is_empty())
    );
}

#[test]
fn memory_status_prefers_graph_db_related_with_claude_mem_fallback() {
    let dir = tempfile::tempdir().unwrap();
    let missing_claude_mem = dir.path().join("missing-claude-mem.db");

    let output = tsift_bin()
        .args([
            "memory",
            "status",
            dir.path().to_str().unwrap(),
            "--claude-mem-db",
            missing_claude_mem.to_str().unwrap(),
            "--json",
        ])
        .output()
        .unwrap();

    assert!(
        output.status.success(),
        "memory status should pass: stderr={}",
        String::from_utf8_lossy(&output.stderr)
    );
    let json: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    let next_commands = json["next_commands"].as_array().unwrap();
    assert!(next_commands.iter().any(|command| {
        command.as_str().unwrap().contains("graph-db --path")
            && command
                .as_str()
                .unwrap()
                .contains(" --json related '<query>'")
    }));
    assert!(next_commands.iter().any(|command| {
        command
            .as_str()
            .unwrap()
            .contains("memory import-claude-mem")
    }));
    let graph_idx = next_commands
        .iter()
        .position(|command| command.as_str().unwrap().contains("graph-db --path"))
        .unwrap();
    let fallback_idx = next_commands
        .iter()
        .position(|command| {
            command
                .as_str()
                .unwrap()
                .contains("memory import-claude-mem")
        })
        .unwrap();
    assert!(graph_idx < fallback_idx);
    assert_eq!(json["claude_mem"]["exists"], false);
    let retirement = &json["claude_mem_retirement"];
    assert_eq!(retirement["decision"], "hold");
    assert_eq!(retirement["direct_reads_allowed"], true);
    assert_eq!(retirement["rollback_until_normal_session_cycle"], true);
    assert!(
        retirement["conditions"]
            .as_array()
            .unwrap()
            .iter()
            .any(|row| { row["name"] == "full_import" && row["status"] == "pass" })
    );
    assert!(
        retirement["conditions"]
            .as_array()
            .unwrap()
            .iter()
            .any(|row| { row["name"] == "semantic_retrieval" && row["status"] == "block" })
    );
    assert!(
        retirement["conditions"]
            .as_array()
            .unwrap()
            .iter()
            .any(|row| { row["name"] == "parity_eval" && row["status"] == "manual_required" })
    );
    assert!(
        retirement["conditions"]
            .as_array()
            .unwrap()
            .iter()
            .any(|row| {
                row["name"] == "normal_session_cycle" && row["status"] == "manual_required"
            })
    );
    assert!(
        retirement["rollback_commands"]
            .as_array()
            .unwrap()
            .iter()
            .any(|command| {
                command
                    .as_str()
                    .unwrap()
                    .contains("memory import-claude-mem")
            })
    );
    assert!(
        retirement["rollback_commands"]
            .as_array()
            .unwrap()
            .iter()
            .any(|command| {
                command.as_str().unwrap().contains("graph-db --path")
                    && command
                        .as_str()
                        .unwrap()
                        .contains(" --json related '<query>'")
            })
    );
}

#[test]
fn memory_query_plan_uses_graph_db_related_with_parent_json_flag() {
    let output = tsift_bin()
        .args(["memory", "query-plan", "memory retrieval", "--json"])
        .output()
        .unwrap();

    assert!(
        output.status.success(),
        "memory query-plan should pass: stderr={}",
        String::from_utf8_lossy(&output.stderr)
    );
    let json: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(json["candidate_limit"], 80);
    let next_commands = json["next_commands"].as_array().unwrap();
    assert!(next_commands.iter().any(|command| {
        command
            .as_str()
            .unwrap()
            .contains("tsift graph-db --path . --json related '<query>'")
    }));
    assert!(!next_commands.iter().any(|command| {
        command
            .as_str()
            .unwrap()
            .contains("related '<query>' --json")
    }));
}

#[test]
fn dci_benchmark_summarizes_memory_retrieval_eval_fixture() {
    let fixture = Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures/memory-retrieval-eval.json");
    assert!(
        fixture.exists(),
        "memory retrieval eval fixture should exist at {}",
        fixture.display()
    );

    let output = tsift_bin()
        .args([
            "dci-benchmark",
            "--fixture",
            fixture.to_str().unwrap(),
            "--json",
        ])
        .output()
        .unwrap();

    assert!(
        output.status.success(),
        "memory retrieval eval should pass: stderr={}",
        String::from_utf8_lossy(&output.stderr)
    );
    let json: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(json["tasks_loaded"], 4);
    assert_eq!(json["strategies_compared"], 3);
    assert_eq!(
        json["strategy_summaries"][0]["strategy"],
        "graph_db_related"
    );
    assert_eq!(json["strategy_summaries"][0]["localized"], 4);
    assert!(
        json["strategy_summaries"][0]["avg_useful_hits"]
            .as_f64()
            .unwrap()
            > 3.0
    );
    assert_eq!(
        json["strategy_summaries"]
            .as_array()
            .unwrap()
            .iter()
            .find(|summary| summary["strategy"] == "claude_mem_api")
            .unwrap()["zero_output_failures"],
        3
    );
    assert!(json["task_rows"].as_array().unwrap().iter().any(|row| {
        row["zero_output_failures"]
            .as_array()
            .unwrap()
            .iter()
            .any(|strategy| strategy == "claude_mem_api")
    }));
    let gate = &json["memory_retrieval_gate"];
    assert_eq!(gate["decision"], "pass");
    assert_eq!(gate["baseline_strategy"], "claude_mem_api");
    assert_eq!(
        gate["candidate_strategies"]
            .as_array()
            .unwrap()
            .iter()
            .map(|strategy| strategy.as_str().unwrap())
            .collect::<Vec<_>>(),
        vec!["tsift_session_review_context_pack", "graph_db_related"]
    );
    assert!(
        gate["rows"]
            .as_array()
            .unwrap()
            .iter()
            .all(|row| row["status"] == "pass")
    );
}

#[test]
fn session_digest_reads_markdown_session_from_stdin() {
    let dir = tempfile::tempdir().unwrap();
    fs::create_dir_all(dir.path().join("src")).unwrap();
    fs::write(dir.path().join("src/lib.rs"), "fn run_sync() {}\n").unwrap();

    let input = "\
❯ Why was this symbol search attempted?
Symbol `run_sync` not found in index.
Error: tsift search timed out after 30s at src/lib.rs:7:9
Verification in `src/tsift`: `cargo test`, `make check`, `cargo build --release`, `cargo install --path . --force`
Committed and pushed in `src/tsift` as `1af09d3` (`feat: add metric run digest`).
do [#sessiondigest]. spec-test-build-install-commit-push
";

    let mut child = tsift_bin()
        .args([
            "session-digest",
            "--json",
            "--path",
            dir.path().to_str().unwrap(),
        ])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    {
        use std::io::Write;
        child
            .stdin
            .as_mut()
            .unwrap()
            .write_all(input.as_bytes())
            .unwrap();
    }
    let output = child.wait_with_output().unwrap();

    assert!(output.status.success(), "session-digest should succeed");
    let json: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(json["source"], "markdown");
    assert_eq!(json["prompt_target_count"], 2);
    assert!(
        json["commands"]
            .as_array()
            .unwrap()
            .iter()
            .any(|command| command["command"] == "cargo test")
    );
    assert!(
        json["touched_symbols"]
            .as_array()
            .unwrap()
            .iter()
            .any(|symbol| symbol["symbol"] == "run_sync")
    );
    assert!(
        json["failures"]
            .as_array()
            .unwrap()
            .iter()
            .any(|failure| failure["kind"] == "timeout")
    );
    assert!(
        json["closeout"]
            .as_array()
            .unwrap()
            .iter()
            .any(|entry| entry["kind"] == "push")
    );
}

#[test]
fn session_digest_ignores_successful_test_summaries_from_stdin() {
    let dir = tempfile::tempdir().unwrap();
    let input = "\
failures:
No failures detected (runner: cargo).
test result: ok. 0 passed; 0 failed; 0 ignored; 0 measured; 1 filtered out; finished in 0.00s
pytest summary: 4 passed, 0 failed in 0.02s
";

    let mut child = tsift_bin()
        .args([
            "session-digest",
            "--json",
            "--path",
            dir.path().to_str().unwrap(),
        ])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    {
        use std::io::Write;
        child
            .stdin
            .as_mut()
            .unwrap()
            .write_all(input.as_bytes())
            .unwrap();
    }
    let output = child.wait_with_output().unwrap();

    assert!(output.status.success(), "session-digest should succeed");
    let json: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(
        json["failures"].as_array().unwrap(),
        &Vec::<serde_json::Value>::new()
    );
}

#[test]
fn session_cost_reads_codex_token_counts_from_stdin() {
    let input = concat!(
        r#"{"timestamp":"2026-05-05T00:00:01Z","provider":"openai","prompt_cache_key":"agent-doc:tsift","routing_affinity":"replica-a","stable_prefix":"agent-doc stable prefix v1","type":"event_msg","payload":{"type":"token_count","info":{"total_token_usage":{"input_tokens":24000,"cached_input_tokens":23000,"output_tokens":300,"reasoning_output_tokens":100,"total_tokens":24300}}}}"#,
        "\n",
        r#"{"timestamp":"2026-05-05T00:00:04Z","provider":"openai","prompt_cache_key":"agent-doc:tsift","routing_affinity":"replica-a","stable_prefix":"agent-doc stable prefix v1","type":"event_msg","payload":{"type":"token_count","info":{"total_token_usage":{"input_tokens":50000,"cached_input_tokens":48000,"output_tokens":650,"reasoning_output_tokens":180,"total_tokens":50650}}}}"#,
        "\n",
        r#"{"timestamp":"2026-05-05T00:00:05Z","provider":"openai","prompt_cache_key":"agent-doc:tsift","routing_affinity":"replica-a","stable_prefix":"agent-doc stable prefix v1","type":"event_msg","payload":{"type":"token_count","info":{"total_token_usage":{"input_tokens":50000,"cached_input_tokens":48000,"output_tokens":650,"reasoning_output_tokens":180,"total_tokens":50650}}}}"#,
        "\n"
    );

    let mut child = tsift_bin()
        .args(["session-cost", "--json"])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    {
        use std::io::Write;
        child
            .stdin
            .as_mut()
            .unwrap()
            .write_all(input.as_bytes())
            .unwrap();
    }
    let output = child.wait_with_output().unwrap();

    assert!(output.status.success(), "session-cost should succeed");
    let json: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(json["source"], "codex_jsonl");
    assert_eq!(json["usage_samples"], 2);
    assert_eq!(json["prompt_tokens"], 50000);
    assert_eq!(json["cached_input_tokens"], 48000);
    assert_eq!(json["output_tokens"], 650);
    assert_eq!(json["total_tokens"], 50650);
    assert_eq!(json["largest_turn_total_tokens"], 26350);
    assert_eq!(json["cached_input_ratio"], 96.0);
    assert_eq!(json["prompt_cache_plan"]["status"], "observed");
    assert_eq!(json["prompt_cache_plan"]["feasible"], true);
    assert_eq!(
        json["prompt_cache_plan"]["observed_cached_input_ratio"],
        "96.00%"
    );
    assert_eq!(json["prompt_cache_plan"]["analytics"]["sample_count"], 2);
    assert_eq!(json["prompt_cache_plan"]["analytics"]["effective"], true);
    assert_eq!(json["prompt_cache_plan"]["analytics"]["trend"], "stable");
    assert_eq!(
        json["prompt_cache_plan"]["analytics"]["average_cached_input_ratio"],
        "96.00%"
    );
    assert_eq!(
        json["prompt_cache_plan"]["analytics"]["first_cached_input_ratio"],
        "95.83%"
    );
    assert_eq!(
        json["prompt_cache_plan"]["analytics"]["last_cached_input_ratio"],
        "96.15%"
    );
    assert_eq!(
        json["prompt_cache_plan"]["analytics"]["cached_input_ratio_delta"],
        "+0.32%"
    );
    assert_eq!(
        json["prompt_cache_plan"]["analytics"]["net_cached_input_tokens"],
        48000
    );
    let scorecard = json["prompt_cache_plan"]["scorecard"].as_array().unwrap();
    assert_eq!(scorecard.len(), 1);
    assert_eq!(scorecard[0]["provider"], "openai");
    assert_eq!(scorecard[0]["sample_count"], 2);
    assert_eq!(scorecard[0]["net_cached_read_tokens"], 48000);
    assert_eq!(scorecard[0]["read_create_ratio"], "read_only");
    assert_eq!(scorecard[0]["trend"], "stable");
    assert_eq!(
        scorecard[0]["suspected_invalidation_cause"],
        "none observed"
    );
    assert_eq!(
        scorecard[0]["next_command"],
        "tsift session-cost --input <session.jsonl> --json"
    );
    assert_eq!(
        json["prompt_cache_plan"]["analytics"]["timeline"][1]["cached_input_ratio"],
        "96.15%"
    );
    assert_eq!(
        json["prompt_cache_plan"]["analytics"]["timeline"][1]["prompt_cache_metadata"]["provider"],
        "openai"
    );
    assert!(
        json["prompt_cache_plan"]["analytics"]["timeline"][1]["prompt_cache_metadata"]
            ["stable_prefix_fingerprint"]
            .as_str()
            .is_some_and(|value| value.starts_with("spfx-"))
    );
    assert!(
        json["prompt_cache_plan"]["provider_adapters"]
            .as_array()
            .unwrap()
            .iter()
            .any(|adapter| {
                adapter["provider"] == "openai" && adapter["status"] == "prompt_cache_key"
            })
    );
    assert!(
        json["prompt_cache_plan"]["provider_adapters"]
            .as_array()
            .unwrap()
            .iter()
            .any(|adapter| {
                adapter["provider"] == "replica_local" && adapter["status"] == "routing_affinity"
            })
    );
    assert!(
        json["prompt_cache_plan"]["actions"]
            .as_array()
            .unwrap()
            .iter()
            .any(|action| action["kind"] == "preserve_cache_shape")
    );
    assert!(
        json["guardrails"]
            .as_array()
            .unwrap()
            .iter()
            .any(|guardrail| guardrail["kind"] == "cache_resend")
    );
}

#[test]
fn session_cost_recommends_prompt_cache_for_large_uncached_prompt() {
    let input = concat!(
        r#"{"timestamp":"2026-05-05T00:00:01Z","type":"event_msg","payload":{"type":"token_count","info":{"total_token_usage":{"input_tokens":20000,"cached_input_tokens":0,"output_tokens":300,"reasoning_output_tokens":50,"total_tokens":20300}}}}"#,
        "\n"
    );

    let mut child = tsift_bin()
        .args(["session-cost", "--json"])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    {
        use std::io::Write;
        child
            .stdin
            .as_mut()
            .unwrap()
            .write_all(input.as_bytes())
            .unwrap();
    }
    let output = child.wait_with_output().unwrap();

    assert!(output.status.success(), "session-cost should succeed");
    let json: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(json["prompt_cache_plan"]["status"], "candidate");
    assert_eq!(json["prompt_cache_plan"]["observed_cached_input_tokens"], 0);
    assert_eq!(json["prompt_cache_plan"]["analytics"]["sample_count"], 1);
    assert_eq!(json["prompt_cache_plan"]["analytics"]["effective"], false);
    assert_eq!(
        json["prompt_cache_plan"]["analytics"]["trend"],
        "single_sample"
    );
    assert_eq!(
        json["prompt_cache_plan"]["analytics"]["average_cached_input_ratio"],
        "0.00%"
    );
    assert!(
        json["prompt_cache_plan"]["invariants"]
            .as_array()
            .unwrap()
            .iter()
            .any(|invariant| invariant
                .as_str()
                .is_some_and(|text| text.contains("append-only")))
    );
    assert!(
        json["prompt_cache_plan"]["provider_adapters"]
            .as_array()
            .unwrap()
            .iter()
            .any(|adapter| {
                adapter["provider"] == "openai" && adapter["status"] == "missing_prompt_cache_key"
            })
    );
    assert!(
        json["prompt_cache_plan"]["provider_adapters"]
            .as_array()
            .unwrap()
            .iter()
            .any(|adapter| {
                adapter["provider"] == "replica_local"
                    && adapter["status"] == "missing_routing_affinity"
            })
    );
    assert!(
        json["prompt_cache_plan"]["actions"]
            .as_array()
            .unwrap()
            .iter()
            .any(|action| action["kind"] == "enable_provider_cache")
    );
    assert!(
        json["prompt_cache_plan"]["actions"]
            .as_array()
            .unwrap()
            .iter()
            .any(|action| action["kind"] == "fix_openai_prompt_cache_key")
    );
}

#[test]
fn session_cost_reports_prompt_cache_invalidation_diagnostics() {
    let input = concat!(
        r#"{"timestamp":"2026-05-05T00:00:01Z","provider":"anthropic","prompt_cache_key":"agent-doc:tsift","routing_affinity":"replica-a","stable_prefix":"agent-doc stable prefix v1","message":{"id":"msg-1","role":"assistant","content":[{"type":"text","text":"ok","cache_control":{"type":"ephemeral"}}],"usage":{"input_tokens":1000,"cache_creation_input_tokens":0,"cache_read_input_tokens":9000,"output_tokens":50}}}"#,
        "\n",
        r#"{"timestamp":"2026-05-05T00:00:02Z","provider":"anthropic","prompt_cache_key":"agent-doc:tsift-cold","routing_affinity":"replica-b","stable_prefix":"agent-doc stable prefix v2","message":{"id":"msg-2","role":"assistant","content":[{"type":"text","text":"ok","cache_control":{"type":"persistent"}}],"usage":{"input_tokens":3000,"cache_creation_input_tokens":6000,"cache_read_input_tokens":1000,"output_tokens":50}}}"#,
        "\n",
        r#"{"timestamp":"2026-05-05T00:00:03Z","provider":"anthropic","prompt_cache_key":"agent-doc:tsift-cold","routing_affinity":"replica-b","stable_prefix":"agent-doc stable prefix v2","message":{"id":"msg-3","role":"assistant","content":[{"type":"text","text":"ok","cache_control":{"type":"persistent"}}],"usage":{"input_tokens":3000,"cache_creation_input_tokens":6000,"cache_read_input_tokens":1000,"output_tokens":50}}}"#,
        "\n",
    );

    let output = run_tsift_stdin(
        &["session-cost", "--source", "claude-jsonl", "--json"],
        input,
    );
    assert!(output.status.success(), "session-cost should succeed");
    let json: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    let diagnostics = json["prompt_cache_plan"]["analytics"]["diagnostics"]
        .as_array()
        .unwrap();
    assert!(diagnostics.iter().any(|diagnostic| {
        diagnostic["kind"] == "cached_ratio_drop" && diagnostic["label"] == "2026-05-05T00:00:02Z"
    }));
    assert!(diagnostics.iter().any(|diagnostic| {
        diagnostic["kind"] == "cache_creation_spike"
            && diagnostic["message"]
                .as_str()
                .is_some_and(|message| message.contains("60.00%"))
    }));
    assert!(diagnostics.iter().any(|diagnostic| {
        diagnostic["kind"] == "read_create_regression"
            && diagnostic["message"]
                .as_str()
                .is_some_and(|message| message.contains("0.92x"))
    }));
    let prefix_drift = json["prompt_cache_plan"]["analytics"]["prefix_drift"]
        .as_array()
        .unwrap();
    assert!(prefix_drift.iter().any(|drift| {
        drift["trigger"] == "cached_ratio_drop_and_cache_creation_spike"
            // Attribution names the most specific concrete cause, not the derived
            // composite fingerprint (#pcacheattr); cache_key is the first changed
            // concrete field here.
            && drift["first_changed_field"] == "cache_key"
            && drift["field_changes"]
                .as_array()
                .unwrap()
                .iter()
                .any(|change| change["field"] == "cache_key")
    }));
    let scorecard = json["prompt_cache_plan"]["scorecard"].as_array().unwrap();
    assert_eq!(scorecard[0]["provider"], "anthropic");
    assert_eq!(scorecard[0]["net_cached_read_tokens"], -1000);
    assert_eq!(scorecard[0]["read_create_ratio"], "0.92x");
    assert_eq!(scorecard[0]["trend"], "declining");
    assert!(
        scorecard[0]["suspected_invalidation_cause"]
            .as_str()
            .is_some_and(|cause| cause.contains("cache_key"))
    );

    let compact = run_tsift_stdin(
        &["session-cost", "--source", "claude-jsonl", "--compact"],
        input,
    );
    assert!(
        compact.status.success(),
        "session-cost compact should succeed"
    );
    let compact_stdout = String::from_utf8_lossy(&compact.stdout);
    assert!(compact_stdout.contains("prompt-cache-diagnostic warn cached_ratio_drop"));
    assert!(compact_stdout.contains("prompt-cache-roi provider:anthropic"));
    assert!(compact_stdout.contains("read_create:0.92x"));
    assert!(compact_stdout.contains("prompt-cache-diagnostic recommend read_create_regression"));
    assert!(
        compact_stdout
            .contains("prompt-cache-prefix-drift warn cached_ratio_drop_and_cache_creation_spike")
    );
    assert!(compact_stdout.contains("prompt-cache-call 2026-05-05T00:00:02Z provider:anthropic"));
    assert!(compact_stdout.contains("fingerprint:spfx-"));
}

#[test]
fn session_cost_prompt_cache_fixture_passes_fail_under() {
    let fixture_path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("fixtures/real-session-prompt-cache-effectiveness.json");
    assert!(
        fixture_path.exists(),
        "real-session prompt-cache fixture should exist at {}",
        fixture_path.display()
    );

    let output = tsift_bin()
        .args([
            "session-cost",
            "--fixture",
            fixture_path.to_str().unwrap(),
            "--fail-under",
            "--json",
        ])
        .output()
        .unwrap();

    assert!(
        output.status.success(),
        "prompt-cache fixture should pass thresholds: stderr={}",
        String::from_utf8_lossy(&output.stderr)
    );
    let json: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert!(json["pass"].as_bool().unwrap());
    assert_eq!(json["totals"]["cases"], 6);
    assert_eq!(json["totals"]["failed"], 0);
    assert!(json["totals"]["net_cached_input_tokens"].as_i64().unwrap() > 380_000);
    assert_eq!(json["totals"]["read_create_regressions"], 0);
    assert_eq!(json["missing_regression_scenarios"], serde_json::json!([]));
    assert_eq!(
        json["covered_regression_scenarios"],
        serde_json::json!([
            "cold_standalone_compaction",
            "openai_prompt_cache_key_churn",
            "replica_routing_churn",
            "volatile_prefix_generated_header"
        ])
    );
    assert!(
        json["cases"]
            .as_array()
            .unwrap()
            .iter()
            .all(|case| case["status"] == "pass")
    );
}

#[test]
fn session_cost_prompt_cache_fixture_fail_under_rejects_missing_adapter_evidence() {
    let dir = tempfile::tempdir().unwrap();
    let fixture_path = dir.path().join("missing-prompt-cache-adapters.json");
    let fixture = serde_json::json!({
        "schema_version": 1,
        "description": "missing adapter evidence",
        "cases": [
            {
                "name": "missing-openai-key",
                "source": "codex-jsonl",
                "minimum_cached_input_ratio": 90.0,
                "minimum_net_cached_input_tokens": 40000,
                "maximum_read_create_regressions": 0,
                "input_lines": [
                    "{\"timestamp\":\"2026-05-05T00:00:01Z\",\"provider\":\"openai\",\"type\":\"event_msg\",\"payload\":{\"type\":\"token_count\",\"info\":{\"total_token_usage\":{\"input_tokens\":24000,\"cached_input_tokens\":23000,\"output_tokens\":300,\"reasoning_output_tokens\":100,\"total_tokens\":24300}}}}",
                    "{\"timestamp\":\"2026-05-05T00:00:04Z\",\"provider\":\"openai\",\"type\":\"event_msg\",\"payload\":{\"type\":\"token_count\",\"info\":{\"total_token_usage\":{\"input_tokens\":50000,\"cached_input_tokens\":48000,\"output_tokens\":650,\"reasoning_output_tokens\":180,\"total_tokens\":50650}}}}"
                ]
            }
        ]
    });
    fs::write(&fixture_path, serde_json::to_string(&fixture).unwrap()).unwrap();

    let output = tsift_bin()
        .args([
            "session-cost",
            "--fixture",
            fixture_path.to_str().unwrap(),
            "--fail-under",
            "--json",
        ])
        .output()
        .unwrap();

    assert!(
        !output.status.success(),
        "prompt-cache fixture should fail missing adapter evidence"
    );
    let json: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert!(!json["pass"].as_bool().unwrap());
    let failures = json["cases"][0]["failures"].as_array().unwrap();
    assert!(failures.iter().any(|failure| {
        failure
            .as_str()
            .is_some_and(|text| text.contains("OpenAI prompt_cache_key"))
    }));
    assert!(failures.iter().any(|failure| {
        failure
            .as_str()
            .is_some_and(|text| text.contains("replica-local routing_affinity"))
    }));
}

#[test]
fn session_cost_reads_codex_last_usage_when_cumulative_streams_interleave() {
    let input = concat!(
        r#"{"timestamp":"2026-05-05T00:00:01Z","type":"event_msg","payload":{"type":"token_count","info":{"total_token_usage":{"input_tokens":1000,"cached_input_tokens":900,"output_tokens":50,"reasoning_output_tokens":10,"total_tokens":1050},"last_token_usage":{"input_tokens":1000,"cached_input_tokens":900,"output_tokens":50,"reasoning_output_tokens":10,"total_tokens":1050}}}}"#,
        "\n",
        r#"{"timestamp":"2026-05-05T00:00:02Z","type":"event_msg","payload":{"type":"token_count","info":{"total_token_usage":{"input_tokens":500,"cached_input_tokens":450,"output_tokens":20,"reasoning_output_tokens":5,"total_tokens":520},"last_token_usage":{"input_tokens":500,"cached_input_tokens":450,"output_tokens":20,"reasoning_output_tokens":5,"total_tokens":520}}}}"#,
        "\n",
        r#"{"timestamp":"2026-05-05T00:00:03Z","type":"event_msg","payload":{"type":"token_count","info":{"total_token_usage":{"input_tokens":1600,"cached_input_tokens":1400,"output_tokens":90,"reasoning_output_tokens":20,"total_tokens":1690},"last_token_usage":{"input_tokens":600,"cached_input_tokens":500,"output_tokens":40,"reasoning_output_tokens":10,"total_tokens":640}}}}"#,
        "\n",
        r#"{"timestamp":"2026-05-05T00:00:04Z","type":"event_msg","payload":{"type":"token_count","info":{"total_token_usage":{"input_tokens":900,"cached_input_tokens":800,"output_tokens":45,"reasoning_output_tokens":10,"total_tokens":945},"last_token_usage":{"input_tokens":400,"cached_input_tokens":350,"output_tokens":25,"reasoning_output_tokens":5,"total_tokens":425}}}}"#,
        "\n",
        r#"{"timestamp":"2026-05-05T00:00:05Z","type":"event_msg","payload":{"type":"token_count","info":{"total_token_usage":{"input_tokens":900,"cached_input_tokens":800,"output_tokens":45,"reasoning_output_tokens":10,"total_tokens":945},"last_token_usage":{"input_tokens":400,"cached_input_tokens":350,"output_tokens":25,"reasoning_output_tokens":5,"total_tokens":425}}}}"#,
        "\n"
    );

    let mut child = tsift_bin()
        .args(["session-cost", "--json"])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    {
        use std::io::Write;
        child
            .stdin
            .as_mut()
            .unwrap()
            .write_all(input.as_bytes())
            .unwrap();
    }
    let output = child.wait_with_output().unwrap();

    assert!(output.status.success(), "session-cost should succeed");
    let json: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(json["usage_samples"], 4);
    assert_eq!(json["prompt_tokens"], 2500);
    assert_eq!(json["cached_input_tokens"], 2200);
    assert_eq!(json["output_tokens"], 135);
    assert_eq!(json["reasoning_output_tokens"], 30);
    assert_eq!(json["total_tokens"], 2635);
    assert_eq!(json["largest_turn_total_tokens"], 1050);
}

#[test]
fn session_cost_summarizes_agent_doc_restart_churn_from_stdin() {
    let input = concat!(
        "[1776528398] codex_start mode=fresh_restart restart_count=1\n",
        "[1776528446] auto_trigger_timeout harness=codex reason=no_prompt_after_30s\n",
        "[1776528450] ctrl_d_restart_fresh restart_count=2\n",
        "[1776528451] user_quit_after_ctrl_d\n",
        "[1776528452] supervisor_exit reason=user_quit_after_ctrl_d pane=%26 restart_count=2\n"
    );

    let mut child = tsift_bin()
        .args(["session-cost", "--json", "--source", "agent-doc-log"])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    {
        use std::io::Write;
        child
            .stdin
            .as_mut()
            .unwrap()
            .write_all(input.as_bytes())
            .unwrap();
    }
    let output = child.wait_with_output().unwrap();

    assert!(output.status.success(), "session-cost should succeed");
    let json: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(json["source"], "agent_doc_log");
    assert_eq!(json["restart_churn_groups"], 4);
    assert_eq!(json["max_restart_count"], 2);
    assert!(
        json["restart_churn"]
            .as_array()
            .unwrap()
            .iter()
            .any(|entry| entry["family"] == "fresh_restart" && entry["occurrences"] == 2)
    );
    assert!(
        json["restart_churn"]
            .as_array()
            .unwrap()
            .iter()
            .any(|entry| entry["family"] == "quit_after_eof" && entry["occurrences"] == 2)
    );
    assert!(
        json["guardrails"]
            .as_array()
            .unwrap()
            .iter()
            .any(|guardrail| guardrail["kind"] == "restart_loop")
    );
}

#[test]
fn session_cost_does_not_warn_restart_loop_for_continue_restart_count() {
    let input = concat!(
        "[1776528398] codex_start mode=continue restart_count=1\n",
        "[1776528450] codex_start mode=continue restart_count=3\n"
    );

    let mut child = tsift_bin()
        .args(["session-cost", "--json", "--source", "agent-doc-log"])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    {
        use std::io::Write;
        child
            .stdin
            .as_mut()
            .unwrap()
            .write_all(input.as_bytes())
            .unwrap();
    }
    let output = child.wait_with_output().unwrap();

    assert!(output.status.success(), "session-cost should succeed");
    let json: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(json["source"], "agent_doc_log");
    assert_eq!(json["max_restart_count"], 3);
    if let Some(guardrails) = json["guardrails"].as_array() {
        assert!(
            guardrails
                .iter()
                .all(|guardrail| guardrail["kind"] != "restart_loop")
        );
    }
}

#[test]
fn session_digest_reads_codex_jsonl_from_stdin() {
    let dir = tempfile::tempdir().unwrap();
    fs::create_dir_all(dir.path().join("src")).unwrap();
    fs::write(dir.path().join("src/lib.rs"), "fn run_sync() {}\n").unwrap();

    let input = concat!(
        r#"{"type":"event_msg","payload":{"type":"user_message","message":"do [#cdxlog]. spec-test-build-install-commit-push"}}"#,
        "\n",
        r#"{"type":"response_item","payload":{"type":"function_call","name":"exec_command","arguments":"{\"cmd\":\"cargo test --manifest-path Cargo.toml\"}"}}"#,
        "\n",
        r#"{"type":"event_msg","payload":{"type":"exec_command_end","exit_code":1,"aggregated_output":"Error: Symbol `run_sync` not found in src/lib.rs:7:9\nCommitted and pushed in `src/tsift` as `943d77d`.","parsed_cmd":[{"type":"unknown","cmd":"cargo test --manifest-path Cargo.toml"}]}}"#,
        "\n"
    );

    let mut child = tsift_bin()
        .args([
            "session-digest",
            "--json",
            "--path",
            dir.path().to_str().unwrap(),
            "--source",
            "codex-jsonl",
        ])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    {
        use std::io::Write;
        child
            .stdin
            .as_mut()
            .unwrap()
            .write_all(input.as_bytes())
            .unwrap();
    }
    let output = child.wait_with_output().unwrap();

    assert!(output.status.success(), "session-digest should succeed");
    let json: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(json["source"], "codex_jsonl");
    assert_eq!(json["prompt_target_count"], 1);
    assert!(
        json["commands"]
            .as_array()
            .unwrap()
            .iter()
            .any(|command| command["command"] == "cargo test --manifest-path Cargo.toml")
    );
    assert!(
        json["touched_symbols"]
            .as_array()
            .unwrap()
            .iter()
            .any(|symbol| symbol["symbol"] == "run_sync")
    );
    assert!(
        json["failures"]
            .as_array()
            .unwrap()
            .iter()
            .any(|failure| failure["kind"] == "exit")
    );
}

#[test]
fn session_digest_filters_codex_jsonl_bogus_file_refs_from_stdin() {
    let dir = tempfile::tempdir().unwrap();
    fs::create_dir_all(dir.path().join("src")).unwrap();
    fs::write(dir.path().join("src/lib.rs"), "fn run_sync() {}\n").unwrap();
    fs::write(dir.path().join("SPEC.md"), "# spec\n").unwrap();

    let input = concat!(
        r#"{"type":"event_msg","payload":{"type":"agent_message","message":"false paths included 2>/dev/null, agent-doc/tsift, digest/session, progress/CI-status, and version/preflight."}}"#,
        "\n",
        r#"{"type":"event_msg","payload":{"type":"exec_command_end","exit_code":0,"aggregated_output":"read src/lib.rs:1 and SPEC.md","parsed_cmd":[{"type":"unknown","cmd":"sed -n '1,20p' src/lib.rs 2>/dev/null"}]}}"#,
        "\n"
    );

    let mut child = tsift_bin()
        .args([
            "session-digest",
            "--json",
            "--path",
            dir.path().to_str().unwrap(),
            "--source",
            "codex-jsonl",
        ])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    {
        use std::io::Write;
        child
            .stdin
            .as_mut()
            .unwrap()
            .write_all(input.as_bytes())
            .unwrap();
    }
    let output = child.wait_with_output().unwrap();

    assert!(output.status.success(), "session-digest should succeed");
    let json: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    let paths = json["touched_files"]
        .as_array()
        .unwrap()
        .iter()
        .map(|file| file["path"].as_str().unwrap())
        .collect::<std::collections::BTreeSet<_>>();

    assert!(paths.contains("src/lib.rs"));
    assert!(paths.contains("SPEC.md"));
    for bogus in [
        "2>/dev/null",
        "agent-doc/tsift",
        "digest/session",
        "progress/CI-status",
        "version/preflight",
    ] {
        assert!(
            !paths.contains(bogus),
            "conversational fragment `{bogus}` should not be a touched file"
        );
    }
}

#[test]
fn session_digest_summarizes_agent_doc_restart_churn_from_stdin() {
    let dir = tempfile::tempdir().unwrap();
    fs::create_dir_all(dir.path().join("tasks/software")).unwrap();
    fs::write(dir.path().join("tasks/software/tsift.md"), "# tsift\n").unwrap();

    let input = concat!(
        "[1776452736] session_start file=tasks/software/tsift.md pane=%141 session=tsift-v0\n",
        "[1776528398] codex_start mode=fresh_restart restart_count=1\n",
        "[1776528446] auto_trigger_timeout harness=codex reason=no_prompt_after_30s\n",
        "[1776528450] ctrl_d_restart_fresh restart_count=2\n",
        "[1776528451] user_quit_after_ctrl_d\n"
    );

    let mut child = tsift_bin()
        .args([
            "session-digest",
            "--json",
            "--path",
            dir.path().to_str().unwrap(),
            "--source",
            "agent-doc-log",
        ])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    {
        use std::io::Write;
        child
            .stdin
            .as_mut()
            .unwrap()
            .write_all(input.as_bytes())
            .unwrap();
    }
    let output = child.wait_with_output().unwrap();

    assert!(output.status.success(), "session-digest should succeed");
    let json: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(json["source"], "agent_doc_log");
    assert_eq!(json["restart_churn_groups"], 4);
    assert!(
        json["restart_churn"]
            .as_array()
            .unwrap()
            .iter()
            .any(|entry| entry["family"] == "ctrl_d_restart_loop" && entry["occurrences"] == 1)
    );
    assert!(
        json["restart_churn"]
            .as_array()
            .unwrap()
            .iter()
            .any(|entry| entry["family"] == "quit_after_eof" && entry["occurrences"] == 1)
    );
}

#[test]
fn session_review_aggregates_cross_harness_logs() {
    let root = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();
    let target = root.path().join("tasks/software/tsift.md");
    fs::create_dir(root.path().join(".git")).unwrap();
    fs::create_dir_all(target.parent().unwrap()).unwrap();
    fs::write(
        &target,
        "---\nagent_doc_session: tsift-v0.1\n---\n\n## Exchange\n",
    )
    .unwrap();

    let agent_doc_logs = root.path().join(".agent-doc/logs");
    fs::create_dir_all(&agent_doc_logs).unwrap();
    fs::write(
        agent_doc_logs.join("tsift-v0.1.log"),
        concat!(
            "[1776712372] session_start file=tasks/software/tsift.md pane=%77 session=tsift-v0.1\n",
            "[1776712373] cwd_resolved path=/tmp/replace-me source=project_root\n",
            "[1776712374] codex_start mode=fresh_restart restart_count=1\n",
            "[1776712375] auto_trigger_timeout harness=codex reason=no_prompt_after_30s\n"
        )
        .replace("/tmp/replace-me", &root.path().display().to_string()),
    )
    .unwrap();

    let claude_dir = home
        .path()
        .join(".claude/projects")
        .join(root.path().display().to_string().replace('/', "-"));
    fs::create_dir_all(&claude_dir).unwrap();
    fs::write(
        claude_dir.join("claude.jsonl"),
        concat!(
            r#"{"cwd":"/tmp/replace-me","message":{"role":"user","content":"do [#ctxpack]. spec-test-build-install-commit-push\nagent-doc /tmp/replace-me/tasks/software/tsift.md"}}"#,
            "\n",
            r#"{"message":{"role":"assistant","id":"msg-1","usage":{"input_tokens":400,"cache_creation_input_tokens":40,"cache_read_input_tokens":360,"output_tokens":25},"content":[{"type":"tool_use","name":"Bash","input":{"command":"cargo test"}},{"type":"text","text":"Verification in `src/tsift`: `cargo test`\nError: Symbol `run_sync` not found in src/lib.rs:7:9"}]}}"#,
            "\n"
        )
        .replace("/tmp/replace-me", &root.path().display().to_string()),
    )
    .unwrap();
    fs::write(
        claude_dir.join("claude-cwd-only.jsonl"),
        concat!(
            r#"{"cwd":"/tmp/replace-me","message":{"role":"user","content":"inspect a different task in this repo"}}"#,
            "\n",
            r#"{"message":{"role":"assistant","id":"msg-2","usage":{"input_tokens":80,"cache_creation_input_tokens":0,"cache_read_input_tokens":60,"output_tokens":10},"content":[{"type":"text","text":"unrelated"}]}}"#,
            "\n"
        )
        .replace("/tmp/replace-me", &root.path().display().to_string()),
    )
    .unwrap();

    let codex_dir = home.path().join(".codex/sessions/2026/05/05");
    fs::create_dir_all(&codex_dir).unwrap();
    fs::write(
        codex_dir.join("rollout-1.jsonl"),
        concat!(
            r#"{"type":"session_meta","payload":{"cwd":"/tmp/replace-me"}}"#,
            "\n",
            r#"{"type":"event_msg","payload":{"type":"user_message","message":"do [#ctxpack]. spec-test-build-install-commit-push\nagent-doc /tmp/replace-me/tasks/software/tsift.md"}}"#,
            "\n",
            r#"{"type":"response_item","payload":{"type":"function_call","name":"exec_command","arguments":"{\"cmd\":\"cargo build --release\"}"}}"#,
            "\n",
            r#"{"timestamp":"2026-05-05T00:00:01Z","type":"event_msg","payload":{"type":"token_count","info":{"total_token_usage":{"input_tokens":1000,"cached_input_tokens":900,"output_tokens":50,"reasoning_output_tokens":10,"total_tokens":1050}}}}"#,
            "\n"
        )
        .replace("/tmp/replace-me", &root.path().display().to_string()),
    )
    .unwrap();
    fs::write(
        codex_dir.join("rollout-cwd-only.jsonl"),
        concat!(
            r#"{"type":"session_meta","payload":{"cwd":"/tmp/replace-me"}}"#,
            "\n",
            r#"{"type":"event_msg","payload":{"type":"user_message","message":"summarize another repo task"}}"#,
            "\n",
            r#"{"timestamp":"2026-05-05T00:00:02Z","type":"event_msg","payload":{"type":"token_count","info":{"total_token_usage":{"input_tokens":200,"cached_input_tokens":150,"output_tokens":20,"reasoning_output_tokens":0,"total_tokens":220}}}}"#,
            "\n"
        )
        .replace("/tmp/replace-me", &root.path().display().to_string()),
    )
    .unwrap();

    let output = tsift_bin()
        .args(["session-review", "--json", target.to_str().unwrap()])
        .env("HOME", home.path())
        .output()
        .unwrap();

    assert!(output.status.success(), "session-review should succeed");
    let json: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(json["target_kind"], "file");
    assert_eq!(json["sessions_matched"], 3);
    assert_eq!(json["sessions_considered"], 5);
    assert_eq!(json["claude_sessions"], 1);
    assert_eq!(json["codex_sessions"], 1);
    assert_eq!(json["agent_doc_logs"], 1);
    assert!(
        json["guardrails"]
            .as_array()
            .unwrap()
            .iter()
            .any(|guardrail| guardrail["kind"] == "restart_loop")
    );
    assert!(
        json["next_context"]["unresolved_failures"]
            .as_array()
            .unwrap()
            .iter()
            .any(|failure| failure["kind"] == "guardrail:restart_loop"
                && failure["message"]
                    .as_str()
                    .is_some_and(|message| message.contains("restart churn detected")))
    );
    assert!(
        json["commands"]
            .as_array()
            .unwrap()
            .iter()
            .any(|command| command["command"] == "cargo test")
    );
    assert!(
        json["commands"]
            .as_array()
            .unwrap()
            .iter()
            .any(|command| command["command"] == "cargo build --release")
    );
    assert_eq!(
        json["next_context"]["last_verification"]["status"],
        "passed"
    );
    assert!(
        json["next_context"]["active_prompt_targets"]
            .as_array()
            .unwrap()
            .iter()
            .any(|prompt| prompt == "do [#ctxpack]. spec-test-build-install-commit-push")
    );
    assert!(
        json["next_context"]["unresolved_failures"]
            .as_array()
            .unwrap()
            .iter()
            .any(|failure| failure["kind"] == "missing" || failure["kind"] == "error")
    );

    let next_context_output = tsift_bin()
        .args([
            "session-review",
            "--next-context",
            "--json",
            target.to_str().unwrap(),
        ])
        .env("HOME", home.path())
        .output()
        .unwrap();
    assert!(
        next_context_output.status.success(),
        "session-review --next-context should succeed"
    );
    let next_context_json: serde_json::Value =
        serde_json::from_slice(&next_context_output.stdout).unwrap();
    assert_eq!(
        next_context_json["target"],
        target.strip_prefix(root.path()).unwrap().to_str().unwrap()
    );
    assert_eq!(
        next_context_json["next_digest_commands"][0],
        "tsift session-review --next-context tasks/software/tsift.md"
    );
}

#[test]
fn session_review_next_context_ignores_successful_test_summaries() {
    let root = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();
    let target = root.path().join("tasks/software/tsift.md");
    fs::create_dir(root.path().join(".git")).unwrap();
    fs::create_dir_all(target.parent().unwrap()).unwrap();
    fs::write(
        &target,
        "---\nagent_doc_session: tsift-v0.1\n---\n\n## Exchange\n",
    )
    .unwrap();

    let codex_dir = home.path().join(".codex/sessions/2026/05/05");
    fs::create_dir_all(&codex_dir).unwrap();
    fs::write(
        codex_dir.join("rollout-success.jsonl"),
        concat!(
            r#"{"type":"session_meta","payload":{"cwd":"/tmp/replace-me"}}"#,
            "\n",
            r#"{"type":"event_msg","payload":{"type":"user_message","message":"do [#sflt]. spec-test-build-install-commit-push\nagent-doc /tmp/replace-me/tasks/software/tsift.md"}}"#,
            "\n",
            r#"{"type":"event_msg","payload":{"type":"agent_message","message":"failures:\nNo failures detected (runner: cargo).\ntest result: ok. 0 passed; 0 failed; 0 ignored; 0 measured; 1 filtered out; finished in 0.00s\nVerification in `src/tsift`: `cargo test` passed."}}"#,
            "\n"
        )
        .replace("/tmp/replace-me", &root.path().display().to_string()),
    )
    .unwrap();

    let output = tsift_bin()
        .args([
            "session-review",
            "--next-context",
            "--json",
            target.to_str().unwrap(),
        ])
        .env("HOME", home.path())
        .output()
        .unwrap();

    assert!(
        output.status.success(),
        "session-review --next-context should succeed"
    );
    let json: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(
        json["unresolved_failures"].as_array().unwrap(),
        &Vec::<serde_json::Value>::new()
    );
}

#[test]
fn session_review_next_context_scopes_live_tail_over_stale_transcript_context() {
    let root = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();
    let target = root.path().join("tasks/software/tsift.md");
    fs::create_dir(root.path().join(".git")).unwrap();
    fs::create_dir_all(target.parent().unwrap()).unwrap();
    fs::write(
        &target,
        "\
---
agent_doc_session: tsift-v0.1
agent_doc_format: template
prompt_presets:
  '#spec-test-build-install-commit-push': update spec + tests. build + install for local testing. commit + push
---

## Exchange

<!-- agent:exchange patch=append -->
### Session Summary

Compacted content:
- Archived old response with do [#stale]. spec-test-build-install-commit-push
<!-- agent:boundary:abc123 -->
do [#active]. spec-test-build-install-commit-push
<!-- /agent:exchange -->

## Queue

<!-- agent:queue preset=\"#spec-test-build-install-commit-push\" go -->
- ~~[#done]~~
- [#active]
- [#later]
<!-- /agent:queue -->

## Backlog

<!-- agent:backlog priority queue -->
- [ ] [#active] Add the active queue profile to context-pack.
- [ ] [#later] Later prompt should remain queued.
- [x] [#done] Completed prompt should stay out of the active profile.
<!-- /agent:backlog -->

## Review

<!-- agent:review -->
- [ ] [#review] Verify the queue profile output.
<!-- /agent:review -->

## Completed / Reaped

<!-- agent:done -->
- 2026-05-12 [#stale] do [#stale]. spec-test-build-install-commit-push
<!-- /agent:done -->
",
    )
    .unwrap();

    let agent_doc_logs = root.path().join(".agent-doc/logs");
    fs::create_dir_all(&agent_doc_logs).unwrap();
    fs::write(
        agent_doc_logs.join("tsift-v0.1.log"),
        concat!(
            "[1776712372] session_start file=tasks/software/tsift.md pane=%77 session=tsift-v0.1\n",
            "[1776712373] cwd_resolved path=/tmp/replace-me source=project_root\n"
        )
        .replace("/tmp/replace-me", &root.path().display().to_string()),
    )
    .unwrap();

    let codex_dir = home.path().join(".codex/sessions/2026/05/05");
    fs::create_dir_all(&codex_dir).unwrap();
    fs::write(
        codex_dir.join("rollout-stale.jsonl"),
        concat!(
            r#"{"type":"session_meta","payload":{"cwd":"/tmp/replace-me"}}"#,
            "\n",
            r#"{"type":"event_msg","payload":{"type":"user_message","message":"do [#stale]. spec-test-build-install-commit-push\nagent-doc /tmp/replace-me/tasks/software/tsift.md"}}"#,
            "\n",
            r####"{"type":"event_msg","payload":{"type":"agent_message","message":"### Re: stale work\nError: old unresolved failure at /!\n`/!` should not be active context"}}"####,
            "\n"
        )
        .replace("/tmp/replace-me", &root.path().display().to_string()),
    )
    .unwrap();

    let output = tsift_bin()
        .args([
            "session-review",
            "--next-context",
            "--json",
            target.to_str().unwrap(),
        ])
        .env("HOME", home.path())
        .output()
        .unwrap();

    assert!(
        output.status.success(),
        "session-review --next-context should succeed"
    );
    let json: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(
        json["active_prompt_targets"],
        serde_json::json!(["do [#active]. spec-test-build-install-commit-push"])
    );
    assert_eq!(
        json["agent_doc_queue"]["active_queue_prompt"],
        serde_json::json!("[#active] Add the active queue profile to context-pack.")
    );
    assert_eq!(
        json["agent_doc_queue"]["live_exchange_tail"],
        serde_json::json!(["do [#active]. spec-test-build-install-commit-push"])
    );
    assert!(
        json["agent_doc_queue"]["backlog_rows"]
            .as_array()
            .unwrap()
            .iter()
            .all(|row| !row.as_str().unwrap().contains("#done"))
    );
    assert!(
        json["agent_doc_queue"]["prompt_presets"]
            .as_array()
            .unwrap()
            .iter()
            .any(|preset| preset
                .as_str()
                .unwrap()
                .starts_with("#spec-test-build-install-commit-push:"))
    );
    assert!(
        json["agent_doc_queue"]["expansion_handles"]
            .as_array()
            .unwrap()
            .iter()
            .any(|handle| handle["expand"].as_str().unwrap().contains("context-pack"))
    );
    assert!(
        json["touched_files"]
            .as_array()
            .unwrap()
            .iter()
            .all(|path| path != "/!")
    );
    assert_eq!(
        json["unresolved_failures"].as_array().unwrap(),
        &Vec::<serde_json::Value>::new()
    );
}

#[test]
fn session_review_next_context_scopes_freeform_live_tail_over_stale_context() {
    let root = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();
    let target = root.path().join("tasks/software/tsift.md");
    fs::create_dir(root.path().join(".git")).unwrap();
    fs::create_dir_all(target.parent().unwrap()).unwrap();
    fs::write(
        &target,
        "\
---
agent_doc_session: tsift-v0.1
agent_doc_format: template
---

## Exchange

<!-- agent:exchange patch=append -->
### Session Summary

Compacted content:
- Archived old response with do [#stale]. spec-test-build-install-commit-push
<!-- agent:boundary:abc123 -->
Evaluate the logs for tsift effectiveness and bugs. #next-steps
<!-- /agent:exchange -->
",
    )
    .unwrap();

    let agent_doc_logs = root.path().join(".agent-doc/logs");
    fs::create_dir_all(&agent_doc_logs).unwrap();
    fs::write(
        agent_doc_logs.join("tsift-v0.1.log"),
        concat!(
            "[1776712372] session_start file=tasks/software/tsift.md pane=%77 session=tsift-v0.1\n",
            "[1776712373] cwd_resolved path=/tmp/replace-me source=project_root\n"
        )
        .replace("/tmp/replace-me", &root.path().display().to_string()),
    )
    .unwrap();

    let codex_dir = home.path().join(".codex/sessions/2026/05/05");
    fs::create_dir_all(&codex_dir).unwrap();
    fs::write(
        codex_dir.join("rollout-stale.jsonl"),
        concat!(
            r#"{"type":"session_meta","payload":{"cwd":"/tmp/replace-me"}}"#,
            "\n",
            r#"{"type":"event_msg","payload":{"type":"user_message","message":"do [#stale]. spec-test-build-install-commit-push\nagent-doc /tmp/replace-me/tasks/software/tsift.md"}}"#,
            "\n",
            r####"{"type":"event_msg","payload":{"type":"agent_message","message":"### Re: stale work\nError: old unresolved failure at /!\n`/!` should not be active context"}}"####,
            "\n"
        )
        .replace("/tmp/replace-me", &root.path().display().to_string()),
    )
    .unwrap();

    let output = tsift_bin()
        .args([
            "session-review",
            "--next-context",
            "--json",
            target.to_str().unwrap(),
        ])
        .env("HOME", home.path())
        .output()
        .unwrap();

    assert!(
        output.status.success(),
        "session-review --next-context should succeed"
    );
    let json: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(
        json["active_prompt_targets"],
        serde_json::json!(["Evaluate the logs for tsift effectiveness and bugs. #next-steps"])
    );
    assert_eq!(
        json["touched_files"].as_array().unwrap(),
        &Vec::<serde_json::Value>::new()
    );
    assert_eq!(
        json["unresolved_failures"].as_array().unwrap(),
        &Vec::<serde_json::Value>::new()
    );
}

#[test]
fn context_pack_json_composes_next_context_and_optional_digests() {
    let root = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();
    fs::create_dir_all(root.path().join("src")).unwrap();
    fs::create_dir_all(root.path().join("tasks/software")).unwrap();
    fs::create_dir_all(root.path().join(".agent-doc/logs")).unwrap();
    fs::create_dir_all(root.path().join(".naming/tags")).unwrap();
    fs::write(
        root.path().join(".naming/tags/alpha.md"),
        "+++\ntag = \"alpha\"\ntitle = \"Alpha Domain\"\ndomain = \"fixture\"\n+++\n\nAlpha definition.\n",
    )
    .unwrap();
    fs::write(
        root.path().join("src/lib.rs"),
        "pub fn alpha() {\n    beta();\n}\n\nfn beta() {}\n",
    )
    .unwrap();
    fs::write(
        root.path().join("tasks/software/tsift.md"),
        "\
---
agent_doc_session: tsift-v0.1
prompt_presets:
  '#spec-test-build-install-commit-push': update spec + tests. commit + push
---

## Exchange

<!-- agent:exchange patch=append -->
### Session Summary

Compacted content:
- Archived stale queue head [#old].
<!-- agent:boundary:ctx -->
<!-- /agent:exchange -->

## Queue

<!-- agent:queue preset=\"#spec-test-build-install-commit-push\" go -->
- ~~[#old]~~
- [#ts1b]
<!-- /agent:queue -->

## Backlog

<!-- agent:backlog priority queue -->
- [ ] [#ts1b] do [#ts1b]. spec-test-build-install-commit-push
- [x] [#old] stale completed prompt
<!-- /agent:backlog -->

## Review

<!-- agent:review -->
- [ ] [#rv1] check context-pack queue profile
<!-- /agent:review -->

## Completed / Reaped

<!-- agent:done -->
- 2026-05-12 [#old] stale completed prompt
<!-- /agent:done -->
",
    )
    .unwrap();
    fs::write(
        root.path().join(".agent-doc/logs/tsift-v0.1.log"),
        format!(
            concat!(
                "[1776712372] session_start file=tasks/software/tsift.md pane=%77 session=tsift-v0.1\n",
                "[1776712373] cwd_resolved path={} source=project_root\n",
                "[1776712374] commit_completed file=tasks/software/tsift.md commit=abc123\n"
            ),
            root.path().display()
        ),
    )
    .unwrap();
    init_git_repo(root.path());

    fs::write(
        root.path().join("src/lib.rs"),
        "pub fn alpha() {\n    beta();\n    gamma();\n}\n\nfn beta() {}\nfn gamma() {}\n",
    )
    .unwrap();
    fs::write(
        root.path().join("target-test.log"),
        "running 2 tests\nthread 'suite::alpha_failure' panicked at src/lib.rs:3:5:\nassertion failed: left == right\nfailures:\n    suite::alpha_failure\n\ntest result: FAILED. 1 passed; 1 failed; 0 ignored; 0 measured; 0 filtered out\n",
    )
    .unwrap();
    fs::write(
        root.path().join("target-build.log"),
        "error: failed to compile fixture\nsrc/lib.rs:3:5: unresolved name gamma\nwarning: retrying build\nwarning: retrying build\n",
    )
    .unwrap();

    let output = tsift_bin()
        .args([
            "context-pack",
            "tasks/software/tsift.md",
            "--json",
            "--test-input",
            "target-test.log",
            "--runner",
            "cargo",
            "--log-input",
            "target-build.log",
            "--max-items",
            "2",
            "--max-bytes",
            "96",
        ])
        .env("HOME", home.path())
        .current_dir(root.path())
        .output()
        .unwrap();

    assert!(output.status.success(), "context-pack should succeed");
    let json: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert!(
        json["target"]
            .as_str()
            .unwrap()
            .ends_with("tasks/software/tsift.md")
    );
    assert!(
        json["next_context"]["target"]
            .as_str()
            .unwrap()
            .ends_with("tasks/software/tsift.md")
    );
    assert_eq!(
        json["next_context"]["agent_doc_queue"]["active_queue_prompt"],
        serde_json::json!("[#ts1b] do [#ts1b]. spec-test-build-install-commit-push")
    );
    assert!(
        json["next_context"]["agent_doc_queue"]["backlog_rows"]
            .as_array()
            .unwrap()
            .iter()
            .all(|row| !row.as_str().unwrap().contains("#old"))
    );
    assert_eq!(
        json["next_context"]["agent_doc_queue"]["review_rows"][0],
        serde_json::json!("[#rv1] check context-pack queue profile")
    );
    assert!(
        json["next_context"]["agent_doc_queue"]["prompt_presets"][0]
            .as_str()
            .unwrap()
            .starts_with("#spec-test-build-install-commit-push:")
    );
    assert!(
        json["next_context"]["agent_doc_queue"]["expansion_handles"]
            .as_array()
            .unwrap()
            .iter()
            .any(|handle| handle["handle"] == "adq-context-pack")
    );
    assert!(
        json["diff_digest"]["files_changed"].as_u64().unwrap() >= 1,
        "expected at least one changed file in diff digest"
    );
    assert_eq!(json["test_digest"]["status"], "included");
    assert_eq!(json["test_digest"]["report"]["runner"], "cargo");
    assert_eq!(json["log_digest"]["status"], "included");
    assert!(
        json["next_context"]["touched_symbol_refs"][0]["handle"]
            .as_str()
            .unwrap()
            .starts_with("ncsym-")
    );
    assert!(
        json["diff_digest"]["files"][0]["touched_symbol_refs"][0]["handle"]
            .as_str()
            .unwrap()
            .starts_with("cdsym-")
    );
    assert_eq!(json["ontology_refs"][0]["tag"], "alpha");
    assert_eq!(json["ontology_refs"][0]["path"], ".naming/tags/alpha.md");
    assert_eq!(
        json["diff_digest"]["files"][0]["touched_symbol_refs"][0]["ontology_refs"][0]["tag"],
        "alpha"
    );
    assert!(
        json["log_digest"]["report"]["symbol_refs"][0]["handle"]
            .as_str()
            .unwrap()
            .starts_with("clsym-")
    );
    assert_eq!(
        json["resume_commands"][0],
        "tsift session-review --next-context tasks/software/tsift.md"
    );
    assert!(
        json["exploration"]["worker_context"][0]["handle"]
            .as_str()
            .unwrap()
            .starts_with("xwrk-")
    );
    assert!(
        json["exploration"]["worker_context"][0]["expand"]
            .as_str()
            .unwrap()
            .contains("context-pack")
    );

    let envelope_output = tsift_bin()
        .args([
            "--envelope",
            "context-pack",
            "tasks/software/tsift.md",
            "--json",
            "--test-input",
            "target-test.log",
            "--runner",
            "cargo",
            "--log-input",
            "target-build.log",
            "--max-items",
            "2",
            "--max-bytes",
            "96",
        ])
        .env("HOME", home.path())
        .current_dir(root.path())
        .output()
        .unwrap();

    assert!(
        envelope_output.status.success(),
        "context-pack envelope should succeed"
    );
    let envelope_json: serde_json::Value = serde_json::from_slice(&envelope_output.stdout).unwrap();
    assert_eq!(envelope_json["tool"], "context-pack");
    assert_eq!(envelope_json["view"], "handoff");
    assert_eq!(
        envelope_json["summary"]["metrics"][0]["label"],
        "prompt_targets"
    );
    assert_eq!(
        envelope_json["report"]["resume_commands"][0],
        "tsift session-review --next-context tasks/software/tsift.md"
    );
}

#[test]
fn log_digest_fixture_gate_passes_token_savings_and_false_negative_thresholds() {
    let fixture_path =
        Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures/log-digest-token-savings.json");
    assert!(
        fixture_path.exists(),
        "log-digest token-savings fixture should exist locally at {}",
        fixture_path.display()
    );

    let output = tsift_bin()
        .args([
            "log-digest",
            "--path",
            ".",
            "--fixture",
            fixture_path.to_str().unwrap(),
            "--fail-under",
            "--json",
        ])
        .output()
        .unwrap();

    assert!(
        output.status.success(),
        "log-digest fixture gate should pass thresholds: stderr={}",
        String::from_utf8_lossy(&output.stderr)
    );
    let json: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert!(json["passed"].as_bool().unwrap());
    assert_eq!(json["failed_cases"], 0);
    let ecosystems = json["cases"]
        .as_array()
        .unwrap()
        .iter()
        .map(|case| case["ecosystem"].as_str().unwrap())
        .collect::<Vec<_>>();
    // All five ecosystems are covered (npm appears twice: an install failure
    // plus a classifier false-positive precision case).
    for ecosystem in ["cargo", "pytest", "npm", "pnpm", "agent-doc"] {
        assert!(
            ecosystems.contains(&ecosystem),
            "fixture should cover the {ecosystem} ecosystem"
        );
    }
    // Every case must compress (savings_ok), keep its real signals, and not
    // misclassify any forbidden benign line as a signal.
    for case in json["cases"].as_array().unwrap() {
        assert!(
            case["savings_ok"].as_bool().unwrap(),
            "case {} should meet savings threshold",
            case["name"]
        );
        assert!(
            case["missing_required_signals"]
                .as_array()
                .unwrap()
                .is_empty(),
            "case {} dropped a required signal",
            case["name"]
        );
        assert!(
            case["present_forbidden_signals"]
                .as_array()
                .unwrap()
                .is_empty(),
            "case {} misclassified a forbidden benign line",
            case["name"]
        );
    }
}

#[test]
fn token_savings_accepts_tagpath_preview_fixture() {
    let fixture_path =
        Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures/tsift-token-savings.json");
    assert!(
        fixture_path.exists(),
        "tagpath token-savings fixture should exist locally at {}",
        fixture_path.display()
    );

    let output = tsift_bin()
        .args([
            "token-savings",
            "--fixture",
            fixture_path.to_str().unwrap(),
            "--fail-under",
            "--json",
        ])
        .output()
        .unwrap();

    assert!(
        output.status.success(),
        "token-savings fixture should pass thresholds: stderr={}",
        String::from_utf8_lossy(&output.stderr)
    );
    let json: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert!(json["pass"].as_bool().unwrap());
    let expected_surfaces = vec![
        "search",
        "explain",
        "session-review",
        "context-pack",
        "normalize-query",
        "ontology-refs",
    ];
    assert_eq!(json["totals"]["cases"], expected_surfaces.len());
    assert_eq!(
        json["cases"]
            .as_array()
            .unwrap()
            .iter()
            .map(|case| case["surface"].as_str().unwrap())
            .collect::<Vec<_>>(),
        expected_surfaces
    );
    let context_pack = json["cases"]
        .as_array()
        .unwrap()
        .iter()
        .find(|case| case["surface"] == "context-pack")
        .expect("context-pack fixture case should be present");
    assert_eq!(context_pack["status"], "pass");
    assert!(
        context_pack["estimated_token_delta"].as_u64().unwrap() > 0,
        "context-pack fixture should prove compact preview savings"
    );
    assert!(
        json["cases"]
            .as_array()
            .unwrap()
            .iter()
            .all(|case| case["status"] == "pass")
    );
    assert!(
        json["totals"]["estimated_token_delta"].as_u64().unwrap() > 0,
        "fixture should prove a positive token delta"
    );
}

#[test]
fn token_savings_accepts_real_session_fixture() {
    let fixture_path =
        Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures/real-session-token-savings.json");
    assert!(
        fixture_path.exists(),
        "real-session token-savings fixture should exist at {}",
        fixture_path.display()
    );

    let output = tsift_bin()
        .args([
            "token-savings",
            "--fixture",
            fixture_path.to_str().unwrap(),
            "--fail-under",
            "--json",
        ])
        .output()
        .unwrap();

    assert!(
        output.status.success(),
        "real-session token-savings fixture should pass thresholds: stderr={}",
        String::from_utf8_lossy(&output.stderr)
    );
    let json: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert!(json["pass"].as_bool().unwrap());
    assert_eq!(json["totals"]["cases"], 3);
    assert_eq!(
        json["cases"]
            .as_array()
            .unwrap()
            .iter()
            .map(|case| case["surface"].as_str().unwrap())
            .collect::<Vec<_>>(),
        vec!["session-review", "context-pack", "source-read"]
    );
    let context_pack = json["cases"]
        .as_array()
        .unwrap()
        .iter()
        .find(|case| case["surface"] == "context-pack")
        .expect("context-pack fixture case should be present");
    assert_eq!(context_pack["status"], "pass");
    assert!(
        context_pack["estimated_token_delta"].as_u64().unwrap() > 0,
        "context-pack fixture should prove markdown projection savings"
    );
    let source_read = json["cases"]
        .as_array()
        .unwrap()
        .iter()
        .find(|case| case["surface"] == "source-read")
        .expect("source-read fixture case should be present");
    assert_eq!(source_read["status"], "pass");
    assert!(
        source_read["estimated_token_delta"].as_u64().unwrap() > 0,
        "source-read fixture should prove bounded read savings"
    );
    assert!(
        json["cases"]
            .as_array()
            .unwrap()
            .iter()
            .all(|case| case["estimated_token_delta"].as_u64().unwrap() > 0)
    );
    assert!(
        json["totals"]["estimated_token_delta"].as_u64().unwrap() > 1000,
        "real-session fixture should prove a large token delta"
    );
}

#[test]
fn session_review_honors_historical_aliases_and_skips_noisy_records() {
    let root = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();
    let target = root.path().join("tasks/software/tsift.md");
    fs::create_dir(root.path().join(".git")).unwrap();
    fs::create_dir_all(target.parent().unwrap()).unwrap();
    fs::write(
        &target,
        "---\nagent_doc_session: tsift-v0.1\n---\n\n## Exchange\n",
    )
    .unwrap();

    let agent_doc_logs = root.path().join(".agent-doc/logs");
    fs::create_dir_all(&agent_doc_logs).unwrap();
    fs::write(
        agent_doc_logs.join("tsift-v0.1.log"),
        concat!(
            "[1776712372] session_start file=tasks/tsift.md pane=%77 session=tsift-v0\n",
            "[1776712373] session_start file=tasks/software/tsift.md pane=%78 session=tsift-v0.1\n",
            "[1776712374] cwd_resolved path=/tmp/replace-me source=project_root\n"
        )
        .replace("/tmp/replace-me", &root.path().display().to_string()),
    )
    .unwrap();

    let claude_dir = home
        .path()
        .join(".claude/projects")
        .join(root.path().display().to_string().replace('/', "-"));
    fs::create_dir_all(&claude_dir).unwrap();
    fs::write(
        claude_dir.join("claude-target.jsonl"),
        concat!(
            "not-json\n",
            r#"{"cwd":"/tmp/replace-me","message":{"role":"user","content":"resume session tsift-v0\nagent-doc tasks/tsift.md"}}"#,
            "\n",
            r#"{"attachment":{"type":"hook_success","content":"tasks/software/tsift.md only in hook output"}}"#,
            "\n"
        )
        .replace("/tmp/replace-me", &root.path().display().to_string()),
    )
    .unwrap();
    fs::write(
        claude_dir.join("claude-noisy.jsonl"),
        concat!(
            r#"{"cwd":"/tmp/replace-me","attachment":{"type":"hook_success","content":"tasks/software/tsift.md only in hook output"}}"#,
            "\n"
        )
        .replace("/tmp/replace-me", &root.path().display().to_string()),
    )
    .unwrap();

    let codex_dir = home.path().join(".codex/sessions/2026/05/05");
    fs::create_dir_all(&codex_dir).unwrap();
    fs::write(
        codex_dir.join("codex-target.jsonl"),
        concat!(
            "not-json\n",
            r#"{"type":"session_meta","payload":{"cwd":"/tmp/replace-me"}}"#,
            "\n",
            r#"{"type":"event_msg","payload":{"type":"user_message","message":"resume tsift-v0\nagent-doc tasks/tsift.md"}}"#,
            "\n",
            r#"{"type":"response_item","payload":{"type":"function_call_output","output":"tasks/software/tsift.md only in output"}}"#,
            "\n"
        )
        .replace("/tmp/replace-me", &root.path().display().to_string()),
    )
    .unwrap();
    fs::write(
        codex_dir.join("codex-noisy.jsonl"),
        concat!(
            r#"{"type":"session_meta","payload":{"cwd":"/tmp/replace-me"}}"#,
            "\n",
            r#"{"type":"response_item","payload":{"type":"function_call_output","output":"tasks/software/tsift.md only in output"}}"#,
            "\n"
        )
        .replace("/tmp/replace-me", &root.path().display().to_string()),
    )
    .unwrap();

    let output = tsift_bin()
        .args(["session-review", "--json", target.to_str().unwrap()])
        .env("HOME", home.path())
        .output()
        .unwrap();

    assert!(output.status.success(), "session-review should succeed");
    let json: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(json["sessions_considered"], 5);
    assert_eq!(json["sessions_matched"], 3);
    assert_eq!(json["claude_sessions"], 1);
    assert_eq!(json["codex_sessions"], 1);
    assert!(json["sessions"].as_array().unwrap().iter().any(|session| {
        session["path"]
            .as_str()
            .unwrap()
            .ends_with("claude-target.jsonl")
            && session["matched_by"]
                .as_array()
                .unwrap()
                .iter()
                .any(|reason| reason == "agent_doc_session" || reason == "path:tasks/tsift.md")
    }));
    assert!(json["warnings"].as_array().unwrap().iter().any(|warning| {
        warning
            .as_str()
            .unwrap()
            .contains("skipping malformed Claude transcript jsonl line 1")
    }));
    assert!(json["warnings"].as_array().unwrap().iter().any(|warning| {
        warning
            .as_str()
            .unwrap()
            .contains("skipping malformed Codex transcript jsonl line 1")
    }));
}

#[test]
fn session_review_json_surfaces_loop_clusters() {
    let root = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();
    let target = root.path().join("tasks/software/tsift.md");
    fs::create_dir(root.path().join(".git")).unwrap();
    fs::create_dir_all(target.parent().unwrap()).unwrap();
    fs::write(
        &target,
        "---\nagent_doc_session: tsift-v0.1\n---\n\n## Exchange\n",
    )
    .unwrap();

    let agent_doc_logs = root.path().join(".agent-doc/logs");
    fs::create_dir_all(&agent_doc_logs).unwrap();
    fs::write(
        agent_doc_logs.join("tsift-v0.1.log"),
        concat!(
            "[1776712372] session_start file=tasks/software/tsift.md pane=%77 session=tsift-v0.1\n",
            "[1776712373] cwd_resolved path=/tmp/replace-me source=project_root\n",
            "[1776712374] commit_already_current file=tasks/software/tsift.md basis=head\n",
            "[1776712375] commit_already_current file=tasks/software/tsift.md basis=head\n",
            "[1776712376] commit_already_current file=tasks/software/tsift.md basis=head\n"
        )
        .replace("/tmp/replace-me", &root.path().display().to_string()),
    )
    .unwrap();

    let codex_dir = home.path().join(".codex/sessions/2026/05/05");
    fs::create_dir_all(&codex_dir).unwrap();
    fs::write(
        codex_dir.join("rollout-1.jsonl"),
        concat!(
            r#"{"type":"session_meta","payload":{"cwd":"/tmp/replace-me"}}"#,
            "\n",
            r#"{"type":"event_msg","payload":{"type":"user_message","message":"do [#looprank]. spec-test-build-install-commit-push\nagent-doc /tmp/replace-me/tasks/software/tsift.md"}}"#,
            "\n",
            r#"{"type":"response_item","payload":{"type":"function_call","name":"exec_command","arguments":"{\"cmd\":\"cargo test\"}"}}"#,
            "\n",
            r#"{"type":"response_item","payload":{"type":"function_call","name":"exec_command","arguments":"{\"cmd\":\"cargo build --release\"}"}}"#,
            "\n",
            r#"{"type":"event_msg","payload":{"type":"agent_message","message":"Committed and pushed in `src/tsift` as `abc123`."}}"#,
            "\n",
            r#"{"type":"event_msg","payload":{"type":"user_message","message":"do [#looprank]. spec-test-build-install-commit-push"}}"#,
            "\n",
            r#"{"type":"response_item","payload":{"type":"function_call","name":"exec_command","arguments":"{\"cmd\":\"cargo test\"}"}}"#,
            "\n",
            r#"{"type":"response_item","payload":{"type":"function_call","name":"exec_command","arguments":"{\"cmd\":\"cargo build --release\"}"}}"#,
            "\n",
            r#"{"type":"event_msg","payload":{"type":"agent_message","message":"Committed and pushed in `src/tsift` as `abc123`."}}"#,
            "\n",
            r#"{"timestamp":"2026-05-05T00:00:01Z","type":"event_msg","payload":{"type":"token_count","info":{"total_token_usage":{"input_tokens":1000,"cached_input_tokens":900,"output_tokens":50,"reasoning_output_tokens":10,"total_tokens":1050},"last_token_usage":{"input_tokens":1000,"cached_input_tokens":900,"output_tokens":50,"reasoning_output_tokens":10,"total_tokens":1050}}}}"#,
            "\n",
            r#"{"timestamp":"2026-05-05T00:00:02Z","type":"event_msg","payload":{"type":"token_count","info":{"total_token_usage":{"input_tokens":500,"cached_input_tokens":450,"output_tokens":20,"reasoning_output_tokens":5,"total_tokens":520},"last_token_usage":{"input_tokens":500,"cached_input_tokens":450,"output_tokens":20,"reasoning_output_tokens":5,"total_tokens":520}}}}"#,
            "\n",
            r#"{"timestamp":"2026-05-05T00:00:03Z","type":"event_msg","payload":{"type":"token_count","info":{"total_token_usage":{"input_tokens":1600,"cached_input_tokens":1400,"output_tokens":90,"reasoning_output_tokens":20,"total_tokens":1690},"last_token_usage":{"input_tokens":600,"cached_input_tokens":500,"output_tokens":40,"reasoning_output_tokens":10,"total_tokens":640}}}}"#,
            "\n",
            r#"{"timestamp":"2026-05-05T00:00:04Z","type":"event_msg","payload":{"type":"token_count","info":{"total_token_usage":{"input_tokens":900,"cached_input_tokens":800,"output_tokens":45,"reasoning_output_tokens":10,"total_tokens":945},"last_token_usage":{"input_tokens":400,"cached_input_tokens":350,"output_tokens":25,"reasoning_output_tokens":5,"total_tokens":425}}}}"#,
            "\n",
            r#"{"timestamp":"2026-05-05T00:00:05Z","type":"event_msg","payload":{"type":"token_count","info":{"total_token_usage":{"input_tokens":900,"cached_input_tokens":800,"output_tokens":45,"reasoning_output_tokens":10,"total_tokens":945},"last_token_usage":{"input_tokens":400,"cached_input_tokens":350,"output_tokens":25,"reasoning_output_tokens":5,"total_tokens":425}}}}"#,
            "\n"
        )
        .replace("/tmp/replace-me", &root.path().display().to_string()),
    )
    .unwrap();

    let output = tsift_bin()
        .args(["session-review", "--json", target.to_str().unwrap()])
        .env("HOME", home.path())
        .output()
        .unwrap();

    assert!(output.status.success(), "session-review should succeed");
    let json: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(json["usage_samples"], 4);
    assert_eq!(json["prompt_tokens"], 2500);
    assert_eq!(json["total_tokens"], 2635);
    assert_eq!(json["largest_turn_total_tokens"], 1050);
    let loop_clusters = json["loop_clusters"].as_array().unwrap();
    assert!(loop_clusters.iter().any(|cluster| {
        cluster["kind"] == "prompt_repeat"
            && cluster["label"] == "do [#looprank]. spec-test-build-install-commit-push"
            && cluster["occurrences"] == 2
    }));
    assert!(loop_clusters.iter().any(|cluster| {
        cluster["kind"] == "command_bundle"
            && cluster["label"] == "cargo test -> cargo build --release"
            && cluster["occurrences"] == 2
    }));
    assert!(loop_clusters.iter().any(|cluster| {
        cluster["kind"] == "closeout_churn"
            && cluster["label"] == "commit_already_current"
            && cluster["occurrences"] == 3
    }));

    let next_context_output = tsift_bin()
        .args([
            "--envelope",
            "session-review",
            "--next-context",
            "--json",
            target.to_str().unwrap(),
        ])
        .env("HOME", home.path())
        .output()
        .unwrap();
    assert!(
        next_context_output.status.success(),
        "session-review --next-context should succeed"
    );
    let next_context_json: serde_json::Value =
        serde_json::from_slice(&next_context_output.stdout).unwrap();
    let next_context_report = next_context_json
        .get("report")
        .unwrap_or(&next_context_json);
    let actions = next_context_report["next_token_actions"]
        .as_array()
        .unwrap_or_else(|| panic!("missing next_token_actions in {next_context_json}"));
    let command_bundle_action = actions
        .iter()
        .find(|action| action["kind"] == "repeated_command_bundle")
        .unwrap_or_else(|| panic!("missing repeated_command_bundle action in {next_context_json}"));
    let rewrite_commands = command_bundle_action["rewrite_commands"]
        .as_array()
        .unwrap_or_else(|| panic!("missing rewrite_commands in {next_context_json}"));
    assert!(
        rewrite_commands
            .iter()
            .any(|command| command.as_str() == Some("tsift rewrite --run \"cargo test\"")),
        "expected cargo test rewrite command in {next_context_json}"
    );
}

#[test]
fn session_review_next_context_collapses_noop_closeout_guidance() {
    let root = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();
    let target = root.path().join("tasks/software/tsift.md");
    fs::create_dir(root.path().join(".git")).unwrap();
    fs::create_dir_all(target.parent().unwrap()).unwrap();
    fs::write(
        &target,
        "---\nagent_doc_session: tsift-v0.1\n---\n\n## Exchange\n",
    )
    .unwrap();

    let agent_doc_logs = root.path().join(".agent-doc/logs");
    fs::create_dir_all(&agent_doc_logs).unwrap();
    fs::write(
        agent_doc_logs.join("tsift-v0.1.log"),
        include_str!("../../../fixtures/session-review/commit-already-current-churn.log"),
    )
    .unwrap();

    let output = tsift_bin()
        .args(["session-review", "--json", target.to_str().unwrap()])
        .env("HOME", home.path())
        .output()
        .unwrap();
    assert!(output.status.success(), "session-review should succeed");
    let json: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert!(
        json["guardrails"]
            .as_array()
            .unwrap()
            .iter()
            .any(|guardrail| {
                guardrail["kind"] == "noop_closeout"
                    && guardrail["message"].as_str().is_some_and(|message| {
                        message.contains("commit_already_current appeared 3 times")
                    })
            })
    );
    assert!(
        json["loop_clusters"]
            .as_array()
            .unwrap()
            .iter()
            .any(|cluster| {
                cluster["kind"] == "closeout_churn"
                    && cluster["label"] == "commit_already_current"
                    && cluster["occurrences"] == 3
            })
    );

    let next_context_output = tsift_bin()
        .args([
            "--envelope",
            "session-review",
            "--next-context",
            "--json",
            target.to_str().unwrap(),
        ])
        .env("HOME", home.path())
        .output()
        .unwrap();
    assert!(
        next_context_output.status.success(),
        "session-review --next-context should succeed"
    );
    let next_context_json: serde_json::Value =
        serde_json::from_slice(&next_context_output.stdout).unwrap();
    let next_context_report = next_context_json
        .get("report")
        .unwrap_or(&next_context_json);
    let actions = next_context_report["next_token_actions"]
        .as_array()
        .unwrap_or_else(|| panic!("missing next_token_actions in {next_context_json}"));
    assert_eq!(
        actions
            .iter()
            .filter(|action| action["kind"] == "noop_closeout")
            .count(),
        1
    );
    assert!(actions.iter().any(|action| {
        action["kind"] == "noop_closeout"
            && action["message"]
                .as_str()
                .is_some_and(|message| message.contains("commit_already_current appeared 3 times"))
    }));
    assert!(
        next_context_report["unresolved_failures"]
            .as_array()
            .unwrap()
            .iter()
            .all(|failure| failure["kind"] != "guardrail:noop_closeout")
    );
}

#[test]
fn session_review_next_context_collapses_actionable_guardrail_failures() {
    let root = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();
    let target = root.path().join("tasks/software/tsift.md");
    fs::create_dir(root.path().join(".git")).unwrap();
    fs::create_dir_all(target.parent().unwrap()).unwrap();
    fs::write(
        &target,
        "---\nagent_doc_session: tsift-v0.1\n---\n\n## Exchange\n",
    )
    .unwrap();

    let root_text = root.path().display().to_string();
    let codex_dir = home.path().join(".codex/sessions/2026/05/05");
    fs::create_dir_all(&codex_dir).unwrap();
    fs::write(
        codex_dir.join("token-action-guardrails.jsonl"),
        include_str!("../../../fixtures/session-review/token-action-guardrails.codex.jsonl")
            .replace("/tmp/replace-me", &root_text),
    )
    .unwrap();

    let agent_doc_logs = root.path().join(".agent-doc/logs");
    fs::create_dir_all(&agent_doc_logs).unwrap();
    fs::write(
        agent_doc_logs.join("tsift-v0.1.log"),
        include_str!("../../../fixtures/session-review/restart-loop.log")
            .replace("/tmp/replace-me", &root_text),
    )
    .unwrap();

    let output = tsift_bin()
        .args(["session-review", "--json", target.to_str().unwrap()])
        .env("HOME", home.path())
        .output()
        .unwrap();
    assert!(output.status.success(), "session-review should succeed");
    let json: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    for kind in ["prompt_budget", "cache_resend", "restart_loop"] {
        assert!(
            json["guardrails"]
                .as_array()
                .unwrap()
                .iter()
                .any(|guardrail| guardrail["kind"] == kind),
            "missing {kind} guardrail in {json}"
        );
        assert!(
            json["next_context"]["unresolved_failures"]
                .as_array()
                .unwrap()
                .iter()
                .any(|failure| failure["kind"] == format!("guardrail:{kind}")),
            "missing unresolved guardrail failure for {kind} in {json}"
        );
    }

    let next_context_output = tsift_bin()
        .args([
            "--envelope",
            "session-review",
            "--next-context",
            "--json",
            target.to_str().unwrap(),
        ])
        .env("HOME", home.path())
        .output()
        .unwrap();
    assert!(
        next_context_output.status.success(),
        "session-review --next-context should succeed"
    );
    let next_context_json: serde_json::Value =
        serde_json::from_slice(&next_context_output.stdout).unwrap();
    let next_context_report = next_context_json
        .get("report")
        .unwrap_or(&next_context_json);
    let actions = next_context_report["next_token_actions"]
        .as_array()
        .unwrap_or_else(|| panic!("missing next_token_actions in {next_context_json}"));
    for kind in ["prompt_budget", "cache_resend", "restart_loop"] {
        assert_eq!(
            actions
                .iter()
                .filter(|action| action["kind"] == kind)
                .count(),
            1,
            "expected exactly one {kind} action in {next_context_json}"
        );
        assert!(
            next_context_report["unresolved_failures"]
                .as_array()
                .unwrap()
                .iter()
                .all(|failure| failure["kind"] != format!("guardrail:{kind}")),
            "actionable {kind} failure should be collapsed in {next_context_json}"
        );
    }
}

#[test]
fn session_review_aggregates_only_visible_bounded_session_rows() {
    let root = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();
    let target = root.path().join("tasks/software/tsift.md");
    fs::create_dir(root.path().join(".git")).unwrap();
    fs::create_dir_all(target.parent().unwrap()).unwrap();
    fs::write(
        &target,
        "---\nagent_doc_session: tsift-v0.1\n---\n\n## Exchange\n",
    )
    .unwrap();

    let codex_dir = home.path().join(".codex/sessions/2026/05/05");
    fs::create_dir_all(&codex_dir).unwrap();
    let root_text = root.path().display().to_string();
    let old_transcript = concat!(
        r#"{"type":"session_meta","payload":{"cwd":"/tmp/replace-me"}}"#,
        "\n",
        r#"{"type":"event_msg","payload":{"type":"user_message","message":"agent-doc /tmp/replace-me/tasks/software/tsift.md"}}"#,
        "\n",
        r#"{"timestamp":"2026-05-05T00:00:00Z","type":"event_msg","payload":{"type":"token_count","info":{"total_token_usage":{"input_tokens":1000000,"cached_input_tokens":900000,"output_tokens":1,"reasoning_output_tokens":0,"total_tokens":1000001},"last_token_usage":{"input_tokens":1000000,"cached_input_tokens":900000,"output_tokens":1,"reasoning_output_tokens":0,"total_tokens":1000001}}}}"#,
        "\n"
    )
    .replace("/tmp/replace-me", &root_text);
    fs::write(codex_dir.join("zz-old-high-token.jsonl"), old_transcript).unwrap();

    for index in 0..12 {
        let transcript = concat!(
            r#"{"type":"session_meta","payload":{"cwd":"/tmp/replace-me"}}"#,
            "\n",
            r#"{"type":"event_msg","payload":{"type":"user_message","message":"agent-doc /tmp/replace-me/tasks/software/tsift.md"}}"#,
            "\n",
            r#"{"timestamp":"2026-05-05T00:00:01Z","type":"event_msg","payload":{"type":"token_count","info":{"total_token_usage":{"input_tokens":100,"cached_input_tokens":80,"output_tokens":10,"reasoning_output_tokens":0,"total_tokens":110},"last_token_usage":{"input_tokens":100,"cached_input_tokens":80,"output_tokens":10,"reasoning_output_tokens":0,"total_tokens":110}}}}"#,
            "\n"
        )
        .replace("/tmp/replace-me", &root_text);
        fs::write(
            codex_dir.join(format!("aa-current-{index:02}.jsonl")),
            transcript,
        )
        .unwrap();
    }

    let output = tsift_bin()
        .args(["session-review", "--json", target.to_str().unwrap()])
        .env("HOME", home.path())
        .output()
        .unwrap();

    assert!(output.status.success(), "session-review should succeed");
    let json: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(json["sessions_considered"], 13);
    assert_eq!(json["sessions_matched"], 12);
    assert_eq!(json["codex_sessions"], 12);
    assert_eq!(json["usage_samples"], 12);
    assert_eq!(json["prompt_tokens"], 1200);
    assert_eq!(json["total_tokens"], 1320);
    assert_eq!(json["largest_turn_total_tokens"], 110);
    assert!(!json["sessions"].as_array().unwrap().iter().any(|session| {
        session["path"]
            .as_str()
            .unwrap()
            .ends_with("zz-old-high-token.jsonl")
    }));
}

#[test]
fn session_review_separates_aggregate_and_latest_session_cost() {
    let root = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();
    let target = root.path().join("tasks/software/tsift.md");
    fs::create_dir(root.path().join(".git")).unwrap();
    fs::create_dir_all(target.parent().unwrap()).unwrap();
    fs::write(
        &target,
        "---\nagent_doc_session: tsift-v0.1\n---\n\n## Exchange\n",
    )
    .unwrap();

    fn codex_transcript(root_text: &str, turns: &[u64]) -> String {
        let mut transcript = String::new();
        transcript.push_str(
            &serde_json::json!({
                "type": "session_meta",
                "payload": { "cwd": root_text }
            })
            .to_string(),
        );
        transcript.push('\n');
        transcript.push_str(
            &serde_json::json!({
                "type": "event_msg",
                "payload": {
                    "type": "user_message",
                    "message": format!("agent-doc {root_text}/tasks/software/tsift.md")
                }
            })
            .to_string(),
        );
        transcript.push('\n');
        let mut cumulative = 0_u64;
        for (index, turn_total) in turns.iter().enumerate() {
            cumulative += turn_total;
            let cached = turn_total.saturating_sub(100);
            let cumulative_cached = cumulative.saturating_sub((index as u64 + 1) * 100);
            transcript.push_str(
                &serde_json::json!({
                    "timestamp": format!("2026-05-05T00:00:{:02}Z", index + 1),
                    "type": "event_msg",
                    "payload": {
                        "type": "token_count",
                        "info": {
                            "total_token_usage": {
                                "input_tokens": cumulative,
                                "cached_input_tokens": cumulative_cached,
                                "output_tokens": 0,
                                "reasoning_output_tokens": 0,
                                "total_tokens": cumulative
                            },
                            "last_token_usage": {
                                "input_tokens": turn_total,
                                "cached_input_tokens": cached,
                                "output_tokens": 0,
                                "reasoning_output_tokens": 0,
                                "total_tokens": turn_total
                            }
                        }
                    }
                })
                .to_string(),
            );
            transcript.push('\n');
        }
        transcript
    }

    let codex_dir = home.path().join(".codex/sessions/2026/05/05");
    fs::create_dir_all(&codex_dir).unwrap();
    let root_text = root.path().display().to_string();
    let mut older_turns = vec![186_897; 305];
    older_turns.push(93_427);
    for (index, chunk) in older_turns.chunks(28).enumerate() {
        fs::write(
            codex_dir.join(format!("bb-older-high-cache-{index:02}.jsonl")),
            codex_transcript(&root_text, chunk),
        )
        .unwrap();
    }
    fs::write(
        codex_dir.join("aa-latest-lower-cost.jsonl"),
        codex_transcript(
            &root_text,
            &[
                67_644, 67_644, 67_644, 67_644, 67_644, 67_644, 67_644, 44_883,
            ],
        ),
    )
    .unwrap();

    let output = tsift_bin()
        .args(["session-review", "--json", target.to_str().unwrap()])
        .env("HOME", home.path())
        .output()
        .unwrap();

    assert!(output.status.success(), "session-review should succeed");
    let json: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(json["sessions_matched"], 12);
    assert_eq!(json["aggregate_cost"]["scope"], "bounded_matched_sessions");
    assert_eq!(json["aggregate_cost"]["sessions"], 12);
    assert_eq!(json["aggregate_cost"]["total_tokens"], 57_615_403);
    assert_eq!(json["aggregate_cost"]["largest_turn_total_tokens"], 186_897);
    assert_eq!(
        json["latest_session_cost"]["scope"],
        "latest_matched_session"
    );
    assert_eq!(json["latest_session_cost"]["sessions"], 1);
    assert_eq!(json["latest_session_cost"]["total_tokens"], 518_391);
    assert_eq!(
        json["latest_session_cost"]["largest_turn_total_tokens"],
        67_644
    );
    assert_eq!(json["sessions"][0]["total_tokens"], 518_391);
    assert_eq!(json["sessions"][0]["largest_turn_total_tokens"], 67_644);
    let scorecard = json["prompt_cache_roi_scorecard"].as_array().unwrap();
    assert_eq!(scorecard.len(), 12);
    assert_eq!(scorecard[0]["session_source"], "codex_jsonl");
    assert_eq!(scorecard[0]["provider"], "openai");
    assert_eq!(scorecard[0]["read_create_ratio"], "read_only");
    assert_eq!(scorecard[0]["trend"], "stable");
    assert!(
        scorecard[0]["session_path"]
            .as_str()
            .is_some_and(|path| path.ends_with("aa-latest-lower-cost.jsonl"))
    );
    assert!(
        scorecard[0]["next_command"]
            .as_str()
            .is_some_and(|command| {
                command.contains("tsift session-cost --source codex-jsonl --input")
                    && command.contains("aa-latest-lower-cost.jsonl")
                    && command.ends_with(" --json")
            })
    );
}

#[test]
fn rewrite_routes_long_agent_doc_reads_to_session_digest() {
    let dir = tempfile::tempdir().unwrap();
    let session = dir.path().join("tsift.md");
    let mut body = String::from("---\nagent_doc_session: tsift-v0.1\n---\n\n## Exchange\n");
    for index in 0..100 {
        body.push_str(&format!("❯ prompt {index}?\n"));
    }
    fs::write(&session, body).unwrap();

    let output = tsift_bin()
        .args(["rewrite", &format!("cat {}", session.to_str().unwrap())])
        .output()
        .unwrap();

    assert!(output.status.success(), "rewrite should succeed");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("tsift session-digest"));
    assert!(stdout.contains("--source markdown"));
    assert!(stdout.contains(session.to_str().unwrap()));
}

#[test]
fn rewrite_routes_submodule_session_reads_to_submodule_digest_root() {
    let dir = tempfile::tempdir().unwrap();
    fs::write(
        dir.path().join(".gitmodules"),
        r#"[submodule "src/tsift"]
	path = src/tsift
	url = https://example.com/tsift
"#,
    )
    .unwrap();
    let submodule = dir.path().join("src/tsift");
    fs::create_dir_all(submodule.join("tasks")).unwrap();
    fs::write(
        submodule.join(".git"),
        "gitdir: ../../.git/modules/src/tsift\n",
    )
    .unwrap();
    let session = submodule.join("tasks/tsift.md");
    let mut body = String::from("---\nagent_doc_session: tsift-v0.1\n---\n\n## Exchange\n");
    for index in 0..100 {
        body.push_str(&format!("❯ prompt {index}?\n"));
    }
    fs::write(&session, body).unwrap();

    let output = tsift_bin()
        .args(["rewrite", &format!("cat {}", session.to_str().unwrap())])
        .output()
        .unwrap();

    assert!(output.status.success(), "rewrite should succeed");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("tsift session-digest"));
    assert!(stdout.contains("--source markdown"));
    assert!(stdout.contains(submodule.to_str().unwrap()));
}

#[test]
fn rewrite_routes_long_codex_jsonl_reads_to_session_digest() {
    let dir = tempfile::tempdir().unwrap();
    let session = dir.path().join("rollout.jsonl");
    let line = r#"{"type":"event_msg","payload":{"type":"user_message","message":"do [#cdxlog]. spec-test-build-install-commit-push"}}"#;
    let body = std::iter::repeat_n(line, 120)
        .collect::<Vec<_>>()
        .join("\n");
    fs::write(&session, format!("{body}\n")).unwrap();

    let output = tsift_bin()
        .args([
            "rewrite",
            &format!("head -n 120 {}", session.to_str().unwrap()),
        ])
        .output()
        .unwrap();

    assert!(output.status.success(), "rewrite should succeed");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("tsift session-digest"));
    assert!(stdout.contains("--source codex-jsonl"));
    assert!(stdout.contains(session.to_str().unwrap()));
}

#[test]
fn rewrite_run_caps_verbose_tsift_search_output() {
    let dir = tempfile::tempdir().unwrap();
    for idx in 0..80 {
        fs::write(
            dir.path().join(format!("match-{idx}.rs")),
            format!("fn hookcaps_{idx}() {{}}\n// hookcaps\n"),
        )
        .unwrap();
    }

    let command = format!(
        "tsift search hookcaps --exact --limit 80 --path {}",
        dir.path().display()
    );
    let output = tsift_bin()
        .args(["rewrite", "--run", &command])
        .output()
        .unwrap();

    assert!(output.status.success(), "rewrite --run should succeed");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(!stdout.contains("Strategy:"));
    assert!(
        stdout.contains("... (+"),
        "expected truncation note in capped output: {stdout}"
    );
    let nonempty_lines = stdout
        .lines()
        .filter(|line| !line.trim().is_empty())
        .count();
    assert!(
        nonempty_lines <= 51,
        "expected capped output, got {nonempty_lines} nonempty lines:\n{stdout}"
    );
}

#[test]
fn rewrite_run_fails_closed_when_no_rewrite_exists() {
    let output = tsift_bin()
        .args(["rewrite", "--run", "printf hello"])
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(1));
    assert!(output.stdout.is_empty());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("no supported tsift rewrite matched this command"),
        "expected no-rewrite reason, got: {stderr}"
    );
    assert!(
        stderr.contains("`--run` executes only rewritten commands"),
        "expected --run guidance, got: {stderr}"
    );
}

#[test]
fn rewrite_rg_files_fails_closed_for_passthrough() {
    let output = tsift_bin()
        .args(["rewrite", "rg --files src/tsift .agent-doc logs"])
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(1));
    assert!(output.stdout.is_empty());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("file-listing commands keep original shell/find/rg semantics"),
        "expected file-listing no-rewrite reason, got: {stderr}"
    );
    assert!(
        stderr.contains("run the original command unchanged"),
        "expected passthrough guidance, got: {stderr}"
    );
}

#[test]
fn rewrite_find_fails_closed_for_passthrough() {
    let output = tsift_bin()
        .args(["rewrite", "find src/tsift .agent-doc -type f -name '*.rs'"])
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(1));
    assert!(output.stdout.is_empty());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("file-listing commands keep original shell/find/rg semantics"),
        "expected find no-rewrite reason, got: {stderr}"
    );
}

#[test]
fn rewrite_redirection_fails_closed_with_reason() {
    let output = tsift_bin()
        .args(["rewrite", "rg authenticate > matches.txt"])
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(1));
    assert!(output.stdout.is_empty());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains(
            "shell metacharacters such as pipes, redirection, or background operators are not rewritten"
        ),
        "expected shell-metacharacter no-rewrite reason, got: {stderr}"
    );
    assert!(
        stderr.contains("run the original command unchanged"),
        "expected passthrough guidance, got: {stderr}"
    );
}

#[test]
fn rewrite_run_envelopes_cargo_test_digest_output_by_default() {
    let dir = tempfile::tempdir().unwrap();
    init_rust_library_crate(dir.path());
    let manifest = dir.path().join("Cargo.toml");
    let command = format!("cargo test --manifest-path {}", manifest.display());

    let output = tsift_bin()
        .current_dir(dir.path())
        .args(["rewrite", "--run", &command])
        .output()
        .unwrap();

    assert!(output.status.success(), "rewrite --run should succeed");
    let json: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(json["tool"], "digest-runner");
    assert_eq!(json["view"], "test-run");
    assert_eq!(json["report"]["command"], command);
    assert_eq!(json["report"]["success"], true);
    assert_eq!(json["report"]["digest"]["failures"], 0);
    assert_eq!(json["summary"]["text"], "test run passed for cargo");
}

#[test]
fn rewrite_run_envelopes_cargo_build_digest_output_by_default() {
    let dir = tempfile::tempdir().unwrap();
    init_rust_library_crate(dir.path());
    let manifest = dir.path().join("Cargo.toml");
    let command = format!("cargo build --manifest-path {}", manifest.display());

    let output = tsift_bin()
        .current_dir(dir.path())
        .args(["rewrite", "--run", &command])
        .output()
        .unwrap();

    assert!(output.status.success(), "rewrite --run should succeed");
    let json: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(json["tool"], "digest-runner");
    assert_eq!(json["view"], "command-run");
    assert_eq!(json["report"]["command"], command);
    assert_eq!(json["report"]["success"], true);
    assert_eq!(json["report"]["digest"]["signal_groups"], 0);
    assert_eq!(
        json["summary"]["text"],
        "command finished without log signals"
    );
}

#[test]
fn digest_runner_preserves_failing_test_exit_code() {
    let dir = tempfile::tempdir().unwrap();
    fs::create_dir_all(dir.path().join("src")).unwrap();
    fs::write(dir.path().join("src/lib.rs"), "fn helper() {}\n").unwrap();

    let shell_command = r#"cat <<'EOF'
running 2 tests
---- tests::alpha stdout ----
thread 'tests::alpha' panicked at src/lib.rs:7:9:
assertion `left == right` failed

test result: FAILED. 0 passed; 1 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.00s
EOF
exit 7"#;

    let output = tsift_bin()
        .args([
            "digest-runner",
            "--kind",
            "test",
            "--runner",
            "cargo",
            "--json",
            "--path",
            dir.path().to_str().unwrap(),
            "--shell-command",
            shell_command,
        ])
        .output()
        .unwrap();

    assert_eq!(output.status.code(), Some(7));
    let json: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(json["runner"], "cargo");
    assert_eq!(json["failures"], 1);
    assert_eq!(json["failure_groups"][0]["path"], "src/lib.rs");
}

#[test]
fn digest_runner_legacy_underscore_alias_still_resolves() {
    // `digest-runner` was promoted from the hidden `__digest-runner` helper; the
    // old name stays a backward-compatible hidden alias for already-emitted
    // rewrites and installed instruction files.
    let dir = tempfile::tempdir().unwrap();
    fs::create_dir_all(dir.path().join("src")).unwrap();
    fs::write(dir.path().join("src/lib.rs"), "fn helper() {}\n").unwrap();

    let shell_command = "printf 'running 1 test\\ntest tests::alpha ... ok\\n\\ntest result: ok. 1 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.00s\\n'";

    let output = tsift_bin()
        .args([
            "--envelope",
            "__digest-runner",
            "--kind",
            "test",
            "--runner",
            "cargo",
            "--json",
            "--path",
            dir.path().to_str().unwrap(),
            "--shell-command",
            shell_command,
        ])
        .output()
        .unwrap();

    assert!(
        output.status.success(),
        "legacy __digest-runner alias should still resolve"
    );
    let json: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(json["tool"], "digest-runner");
    assert_eq!(json["view"], "test-run");
    assert_eq!(json["report"]["success"], true);
}

#[test]
fn digest_runner_envelope_persists_artifact_for_green_test_runs() {
    let dir = tempfile::tempdir().unwrap();
    fs::create_dir_all(dir.path().join("src")).unwrap();
    fs::write(dir.path().join("src/lib.rs"), "fn helper() {}\n").unwrap();

    let shell_command = "printf 'running 1 test\\ntest tests::alpha ... ok\\n\\ntest result: ok. 1 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.00s\\n'";

    let output = tsift_bin()
        .args([
            "--envelope",
            "digest-runner",
            "--kind",
            "test",
            "--runner",
            "cargo",
            "--json",
            "--path",
            dir.path().to_str().unwrap(),
            "--shell-command",
            shell_command,
        ])
        .output()
        .unwrap();

    assert!(output.status.success());
    let json: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(json["tool"], "digest-runner");
    assert_eq!(json["view"], "test-run");
    assert_eq!(json["report"]["success"], true);
    assert_eq!(json["report"]["digest"]["failures"], 0);
    assert_eq!(json["summary"]["text"], "test run passed for cargo");
    let artifact_root = std::path::Path::new(json["report"]["digest"]["root"].as_str().unwrap());
    let artifact_path = artifact_root.join(json["report"]["artifact"]["path"].as_str().unwrap());
    assert!(artifact_path.exists(), "artifact should be written to disk");
    let artifact_body = fs::read_to_string(&artifact_path).unwrap();
    assert!(artifact_body.contains("test result: ok."));
    assert!(
        json["follow_up"][0]
            .as_str()
            .unwrap()
            .contains("tsift test-digest")
    );
    assert!(json["follow_up"][0].as_str().unwrap().contains("--runner"));
}

#[test]
fn digest_runner_captures_stderr_for_log_digest() {
    let dir = tempfile::tempdir().unwrap();
    fs::create_dir_all(dir.path().join("src")).unwrap();
    fs::write(dir.path().join("src/lib.rs"), "fn run_sync() {}\n").unwrap();

    let output = tsift_bin()
        .args([
            "digest-runner",
            "--kind",
            "log",
            "--json",
            "--path",
            dir.path().to_str().unwrap(),
            "--shell-command",
            "printf 'error: run_sync failed at src/lib.rs:1:1\n' >&2; exit 3",
        ])
        .output()
        .unwrap();

    assert_eq!(output.status.code(), Some(3));
    let json: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(json["signal_groups"], 1);
    assert_eq!(json["file_refs"][0]["path"], "src/lib.rs");
}

#[test]
#[cfg(unix)]
fn digest_runner_delegates_supported_commands_to_rtk_and_keeps_envelope_metadata() {
    let dir = tempfile::tempdir().unwrap();
    fs::create_dir_all(dir.path().join("src")).unwrap();
    fs::write(dir.path().join("src/lib.rs"), "fn helper() {}\n").unwrap();

    let fake_bin = tempfile::tempdir().unwrap();
    let rtk_path = fake_bin.path().join("rtk");
    fs::write(
        &rtk_path,
        r#"#!/bin/sh
if [ "$1" = "rewrite" ]; then
  shift
  if [ "$*" = "cargo build --quiet" ]; then
    printf 'rtk cargo build --quiet'
    exit 0
  fi
  exit 1
fi
if [ "$1" = "cargo" ] && [ "$2" = "build" ]; then
  printf 'rtk compact build ok\n'
  exit 0
fi
exit 2
"#,
    )
    .unwrap();
    let mut permissions = fs::metadata(&rtk_path).unwrap().permissions();
    permissions.set_mode(0o755);
    fs::set_permissions(&rtk_path, permissions).unwrap();
    let path = format!(
        "{}:{}",
        fake_bin.path().display(),
        std::env::var("PATH").unwrap_or_default()
    );

    let output = tsift_bin()
        .env("PATH", path)
        .args([
            "--envelope",
            "digest-runner",
            "--kind",
            "log",
            "--json",
            "--path",
            dir.path().to_str().unwrap(),
            "--shell-command",
            "cargo build --quiet",
        ])
        .output()
        .unwrap();

    assert!(output.status.success());
    let json: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(json["tool"], "digest-runner");
    assert_eq!(json["report"]["command"], "cargo build --quiet");
    assert_eq!(
        json["report"]["executed_command"],
        "rtk cargo build --quiet"
    );
    assert_eq!(json["report"]["filter"]["tool"], "rtk");
    assert_eq!(
        json["report"]["filter"]["command"],
        "rtk cargo build --quiet"
    );
    assert_eq!(json["summary"]["metrics"][1]["label"], "filter");
    assert_eq!(json["summary"]["metrics"][1]["value"], "rtk");
    let artifact_root = std::path::Path::new(json["report"]["digest"]["root"].as_str().unwrap());
    let artifact_path = artifact_root.join(json["report"]["artifact"]["path"].as_str().unwrap());
    assert!(artifact_path.exists(), "artifact should be written to disk");
    let artifact_body = fs::read_to_string(&artifact_path).unwrap();
    assert!(artifact_body.contains("rtk compact build ok"));
}

#[test]
fn search_worker_uses_stable_tsift_cache_dir() {
    let dir = tempfile::tempdir().unwrap();
    fs::write(dir.path().join("main.rs"), "fn main() {}").unwrap();

    // #015t Phase 4: the stable `search-cache` token index is the LEGACY path; force
    // it on (`TSIFT_FTS_SEARCH=0`) so this still validates the cache-dir reuse it
    // was written for. The default FTS5 path does not use `search-cache`.
    let output = tsift_bin()
        .env("TSIFT_FTS_SEARCH", "0")
        .args(["search", "--path", dir.path().to_str().unwrap(), "main"])
        .output()
        .unwrap();

    assert!(output.status.success(), "search should succeed");
    assert!(
        dir.path().join(".tsift/search-cache").exists(),
        "timed worker search should reuse the stable .tsift/search-cache dir"
    );
}

#[test]
fn identifier_like_default_search_uses_exact_backend_without_index() {
    let dir = tempfile::tempdir().unwrap();
    fs::write(
        dir.path().join("notes.md"),
        "workspace anchor: claudescore-3\n",
    )
    .unwrap();

    let output = tsift_bin()
        .args([
            "search",
            "--path",
            dir.path().to_str().unwrap(),
            "--json",
            "claudescore-3",
        ])
        .output()
        .unwrap();

    assert!(
        output.status.success(),
        "identifier-like query should succeed"
    );
    let json: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(json["strategy"], "exact");
    assert_eq!(json["hits"].as_array().unwrap().len(), 1);
    assert_eq!(json["hits"][0]["path"], "notes.md");
    assert!(
        !dir.path().join(".tsift/index.db").exists(),
        "auto-exact routing should not build or require an index"
    );
}

#[test]
fn exact_search_human_output_collapses_repeated_hits_by_file() {
    let dir = tempfile::tempdir().unwrap();
    fs::write(
        dir.path().join("notes.md"),
        "claudescore-3 a\nclaudescore-3 b\nclaudescore-3 c\n",
    )
    .unwrap();
    fs::write(dir.path().join("other.md"), "claudescore-3 d\n").unwrap();

    let output = tsift_bin()
        .args([
            "search",
            "--path",
            dir.path().to_str().unwrap(),
            "claudescore-3",
        ])
        .output()
        .unwrap();

    assert!(output.status.success(), "search should succeed");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("File matches (2 files / 4 hits):"),
        "{stdout}"
    );
    assert!(stdout.contains("notes.md (hits: 3"), "{stdout}");
    assert!(stdout.contains("(+1 more hits in file)"), "{stdout}");
}

#[test]
fn workflow_search_json_documents_handle_preserving_recipe() {
    let output = tsift_bin()
        .args(["workflow", "search", "--json"])
        .output()
        .unwrap();

    assert!(output.status.success(), "workflow should succeed");
    let json: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(json["topic"], "search");
    let steps = json["steps"].as_array().unwrap();
    let names: Vec<&str> = steps
        .iter()
        .map(|step| step["name"].as_str().unwrap())
        .collect();
    assert_eq!(
        names,
        vec![
            "exact-anchor",
            "semantic-search",
            "explain-symbol",
            "summarize-selection",
            "digest-expansion"
        ]
    );
    assert!(
        json["handle_contract"]
            .as_array()
            .unwrap()
            .iter()
            .any(|item| item.as_str().unwrap().contains("originating command")),
        "{json}"
    );
    assert!(
        steps.iter().any(|step| step["command"]
            .as_str()
            .unwrap()
            .contains("tsift --envelope explain")),
        "{json}"
    );
    assert!(
        steps.iter().any(|step| step["preserves"]
            .as_array()
            .unwrap()
            .iter()
            .any(|item| item.as_str().unwrap().contains("artifact handles"))),
        "{json}"
    );
}

#[test]
fn explain_human_output_collapses_dense_edges_by_file() {
    let dir = indexed_cli_fixture();

    let output = tsift_bin()
        .args(["explain", "alpha", dir.path().to_str().unwrap()])
        .output()
        .unwrap();

    assert!(output.status.success(), "explain should succeed");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("Callers (3):"), "{stdout}");
    assert!(
        stdout.contains("main.rs (3): main, beta, gamma"),
        "{stdout}"
    );
}

#[test]
fn edit_intents_structural_rewrite_applies_pattern_codemod() {
    let dir = git_indexed_cli_fixture();
    let input = r#"{
        "intents": [
            {
                "kind": "structural_rewrite",
                "file": "main.rs",
                "pattern": "alpha()",
                "replacement": "alpha_v2()"
            }
        ]
    }"#;
    let output = run_tsift_stdin(
        &[
            "--envelope",
            "edit-intents",
            "--path",
            dir.path().to_str().unwrap(),
            "--apply",
            "--json",
        ],
        input,
    );
    assert!(
        output.status.success(),
        "structural_rewrite stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let json: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    let plan = &json["report"]["plans"][0];
    assert_eq!(plan["kind"], "structural_rewrite");
    assert_eq!(plan["status"], "applied");
    assert_eq!(json["report"]["applied_total"], 1);
    // The pattern-driven plan still carries the same patch contract as a
    // symbol-resolved intent — that is the point of routing it through
    // edit-intents instead of leaving it in `tsift ast-grep rewrite`.
    assert!(
        plan["patch_proposal"]["files"]
            .as_array()
            .is_some_and(|files| !files.is_empty()),
        "structural plan should carry a patch proposal: {plan}"
    );

    let source = fs::read_to_string(dir.path().join("main.rs")).unwrap();
    // Every call site is rewritten, and the definition (a function_item, not a
    // call_expression) is left alone.
    assert_eq!(source.matches("alpha_v2();").count(), 3, "{source}");
    assert!(source.contains("fn alpha() {"), "{source}");
    assert!(!source.contains("    alpha();"), "{source}");
}

#[test]
fn edit_intents_structural_rewrite_reaches_the_kotlin_and_bash_executors() {
    // Kotlin and Bash have an ast-grep grammar and graph symbol extraction, so
    // structural_rewrite reaches them with the full planner contract, and so
    // does `rename_symbol` (covered by
    // `edit_intents_apply_renames_bash_zig_and_gdscript_symbols`). The kinds
    // that still need language-specific rewriting — `replace_function_body`,
    // `insert_import`, `add_method` — must be refused rather than fall through
    // to the Rust implementations.
    for (file, body, pattern, replacement, expect_after, absent_after) in [
        (
            "Main.kt",
            "fun main() {\n    foo(1)\n    foo(2)\n}\n",
            "foo($A)",
            "bar($A)",
            "bar(1)",
            "foo(1)",
        ),
        (
            "run.sh",
            // Wrapped in a function so `main` is an indexed symbol here too —
            // the refusal below must come from the executor, not from symbol
            // resolution failing first.
            "main() {\n  foo 1\n  foo 2\n}\n",
            "foo $A",
            "bar $A",
            "bar 1",
            "foo 1",
        ),
    ] {
        let dir = tempfile::tempdir().unwrap();
        // The file has to exist before `init_git_repo` — it commits, and an
        // empty tree has nothing to commit.
        fs::write(dir.path().join(file), body).unwrap();
        init_git_repo(dir.path());
        // Symbol-resolved kinds need the index, so the refusal below has to be
        // the executor's, not a missing-index error standing in for it.
        let indexed = tsift_bin()
            .args(["index", dir.path().to_str().unwrap()])
            .output()
            .unwrap();
        assert!(
            indexed.status.success(),
            "{file} index stderr: {}",
            String::from_utf8_lossy(&indexed.stderr)
        );

        let input = format!(
            r#"{{"intents": [{{"kind": "structural_rewrite", "file": "{file}",
                 "pattern": "{pattern}", "replacement": "{replacement}"}}]}}"#
        );
        let output = run_tsift_stdin(
            &[
                "--envelope",
                "edit-intents",
                "--path",
                dir.path().to_str().unwrap(),
                "--apply",
                "--json",
            ],
            &input,
        );
        assert!(
            output.status.success(),
            "{file} structural_rewrite stderr: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        let json: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
        let plan = &json["report"]["plans"][0];
        assert_eq!(plan["status"], "applied", "{file}: {plan}");
        assert_eq!(json["report"]["applied_total"], 1, "{file}");
        assert!(
            plan["patch_proposal"]["files"]
                .as_array()
                .is_some_and(|files| !files.is_empty()),
            "{file}: structural plan should carry a patch proposal: {plan}"
        );

        let after = fs::read_to_string(dir.path().join(file)).unwrap();
        assert!(after.contains(expect_after), "{file}: {after}");
        assert!(!after.contains(absent_after), "{file}: {after}");

        // A kind this tier has no rewriting for is refused, and refused
        // *without writing*.
        let before = after.clone();
        let refused = run_tsift_stdin(
            &[
                "--envelope",
                "edit-intents",
                "--path",
                dir.path().to_str().unwrap(),
                "--apply",
                "--json",
            ],
            &format!(
                r#"{{"intents": [{{"kind": "replace_function_body", "file": "{file}",
                     "symbol": "main", "replacement": "return 0"}}]}}"#
            ),
        );
        let combined = format!(
            "{}{}",
            String::from_utf8_lossy(&refused.stdout),
            String::from_utf8_lossy(&refused.stderr)
        );
        assert!(
            combined.contains("is not supported by the"),
            "{file}: expected an unsupported-kind refusal, got: {combined}"
        );
        assert_eq!(
            fs::read_to_string(dir.path().join(file)).unwrap(),
            before,
            "{file}: a refused intent must not write"
        );
    }
}

#[test]
fn edit_intents_structural_rewrite_dry_run_reports_diff_without_writing() {
    let dir = git_indexed_cli_fixture();
    let before = fs::read_to_string(dir.path().join("main.rs")).unwrap();
    let input = r#"{
        "intents": [
            {
                "kind": "structural_rewrite",
                "file": "main.rs",
                "pattern": "alpha()",
                "replacement": "alpha_v2()"
            }
        ]
    }"#;
    let output = run_tsift_stdin(
        &[
            "--envelope",
            "edit-intents",
            "--path",
            dir.path().to_str().unwrap(),
            "--json",
            "--budget",
            "normal",
        ],
        input,
    );
    assert!(
        output.status.success(),
        "structural_rewrite dry-run stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let json: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    let plan = &json["report"]["plans"][0];
    assert_eq!(plan["status"], "planned");
    assert_eq!(plan["apply_supported"], true);
    assert_eq!(plan["applied"], false);
    assert!(
        plan["diff"].as_str().is_some_and(|diff| !diff.is_empty()),
        "dry run must show the mutation: {plan}"
    );
    assert_eq!(
        fs::read_to_string(dir.path().join("main.rs")).unwrap(),
        before,
        "dry run must not touch the working tree"
    );
}

#[test]
fn edit_intents_structural_rewrite_refuses_a_pattern_with_no_match() {
    let dir = git_indexed_cli_fixture();
    let input = r#"{
        "intents": [
            {
                "kind": "structural_rewrite",
                "file": "main.rs",
                "pattern": "nonexistent_call()",
                "replacement": "still_nothing()"
            }
        ]
    }"#;
    let output = run_tsift_stdin(
        &[
            "--envelope",
            "edit-intents",
            "--path",
            dir.path().to_str().unwrap(),
            "--json",
        ],
        input,
    );
    let json: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    let plan = &json["report"]["plans"][0];
    assert_eq!(plan["status"], "unsupported");
    assert_eq!(plan["apply_supported"], false);
    assert!(
        plan["message"]
            .as_str()
            .unwrap()
            .contains("matched nothing"),
        "{plan}"
    );
}

#[test]
fn edit_intents_structural_rewrite_verify_uses_temp_worktree_without_mutating_source() {
    // The whole reason this intent kind exists: a pattern-driven codemod that
    // gets the temp-worktree reindex/impact proof the ad-hoc
    // `tsift ast-grep rewrite --apply` path cannot offer.
    let dir = git_indexed_cli_fixture();
    let before = fs::read_to_string(dir.path().join("main.rs")).unwrap();
    let input = r#"{
        "intents": [
            {
                "kind": "structural_rewrite",
                "file": "main.rs",
                "pattern": "alpha()",
                "replacement": "alpha_v2()"
            }
        ]
    }"#;
    let output = run_tsift_stdin(
        &[
            "--envelope",
            "edit-intents",
            "--path",
            dir.path().to_str().unwrap(),
            "--json",
            "--verify",
            "--budget",
            "normal",
        ],
        input,
    );
    assert!(
        output.status.success(),
        "structural_rewrite --verify stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let json: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(json["report"]["mode"], "verify");
    assert_eq!(json["report"]["applied_total"], 0);
    assert_eq!(json["report"]["verification"]["status"], "passed");
    assert_eq!(json["report"]["verification"]["temp_applied_total"], 1);
    assert_eq!(json["report"]["verification"]["reindexed"], true);
    assert_eq!(
        fs::read_to_string(dir.path().join("main.rs")).unwrap(),
        before,
        "verify must leave the real tree untouched"
    );
}

#[test]
fn structural_only_language_rewrites_via_ast_grep_and_via_edit_intents() {
    // The structural-only tier: Java has an ast-grep grammar but no tsift-graph
    // tag queries, so it is not indexed or searchable. Reparsing a rewritten
    // buffer needs a grammar and not an index, so it *is* a semantic-edit
    // executor for `structural_rewrite` — both surfaces must rewrite it, and
    // the planner path must additionally leave the file untouched until
    // `--apply`. (#goindex moved Go out of this tier; Java is the exemplar now.)
    let dir = tempfile::tempdir().unwrap();
    let src = "class Main {\n    void run() {\n        foo(1);\n        foo(2);\n    }\n}\n";
    fs::write(dir.path().join("Main.java"), src).unwrap();

    let rewrite = tsift_bin()
        .args([
            "ast-grep",
            "rewrite",
            "foo($A)",
            "bar($A)",
            "--path",
            dir.path().to_str().unwrap(),
            "--json",
        ])
        .output()
        .unwrap();
    assert!(
        rewrite.status.success(),
        "ast-grep rewrite on Java stderr: {}",
        String::from_utf8_lossy(&rewrite.stderr)
    );
    let rewrite_json: serde_json::Value = serde_json::from_slice(&rewrite.stdout).unwrap();
    let matched = rewrite_json.to_string();
    assert!(
        matched.contains("bar($A)") || matched.contains("bar(1)"),
        "Java should be structurally rewritable: {rewrite_json}"
    );

    let index = tsift_bin()
        .args(["index", dir.path().to_str().unwrap()])
        .output()
        .unwrap();
    assert!(index.status.success());

    let input = r#"{
        "intents": [
            {
                "kind": "structural_rewrite",
                "file": "Main.java",
                "pattern": "foo($A)",
                "replacement": "bar($A)"
            }
        ]
    }"#;
    let output = run_tsift_stdin(
        &[
            "--envelope",
            "edit-intents",
            "--path",
            dir.path().to_str().unwrap(),
            "--json",
        ],
        input,
    );
    let json: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    let plan = &json["report"]["plans"][0];
    assert_eq!(plan["status"], "planned", "{plan}");
    assert_eq!(plan["apply_supported"], true, "{plan}");
    assert!(
        plan["message"].as_str().unwrap().contains("Java"),
        "the plan should name the Java executor: {plan}"
    );
    assert_eq!(
        fs::read_to_string(dir.path().join("Main.java")).unwrap(),
        src,
        "a dry-run structural_rewrite must not write"
    );

    // A symbol-resolved kind stays refused end to end. Two independent layers
    // stop it: Java has no tag queries, so nothing resolves the symbol, and the
    // Java contract does not recognize the kind either — which is what keeps the
    // family split from routing it into the Rust implementations. The
    // executor-level refusal is asserted per language in the unit suite; here
    // the point is that the CLI fails and writes nothing.
    let rename = r#"{
        "intents": [
            {
                "kind": "rename_symbol",
                "file": "Main.java",
                "symbol": "run",
                "new_name": "execute"
            }
        ]
    }"#;
    let output = run_tsift_stdin(
        &[
            "--envelope",
            "edit-intents",
            "--apply",
            "--path",
            dir.path().to_str().unwrap(),
            "--json",
        ],
        rename,
    );
    assert!(
        !output.status.success(),
        "rename_symbol on Java should fail, stdout: {}",
        String::from_utf8_lossy(&output.stdout)
    );
    assert_eq!(
        fs::read_to_string(dir.path().join("Main.java")).unwrap(),
        src,
        "a refused rename_symbol must not write"
    );

    // And apply actually mutates it.
    let output = run_tsift_stdin(
        &[
            "--envelope",
            "edit-intents",
            "--apply",
            "--path",
            dir.path().to_str().unwrap(),
            "--json",
        ],
        input,
    );
    assert!(
        output.status.success(),
        "apply on Java stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let applied = fs::read_to_string(dir.path().join("Main.java")).unwrap();
    assert!(
        applied.contains("bar(1)") && applied.contains("bar(2)"),
        "apply should rewrite every Java call site: {applied}"
    );
}

/// Bash, Zig, and GDScript sources plus a GDScript caller in a second file.
///
/// The bash source carries the case that makes bash different from every other
/// indexed language: `echo widget_count` is an unquoted argument, which the
/// grammar spells with the same `word` node as a function name, and it is data.
fn indexed_tier_rename_fixture() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    fs::write(
        dir.path().join("util.sh"),
        "widget_count() {\n  echo widget_count\n  local label=\"widget_count\"\n  # widget_count comment\n  return 3\n}\n",
    )
    .unwrap();
    fs::write(
        dir.path().join("main.zig"),
        "// widget_count comment\npub fn widget_count() u32 {\n    const label = \"widget_count\";\n    _ = label;\n    return 3;\n}\n",
    )
    .unwrap();
    fs::write(
        dir.path().join("player.gd"),
        "# widget_count comment\nfunc widget_count():\n\tvar label = \"widget_count\"\n\treturn label\n",
    )
    .unwrap();
    fs::write(
        dir.path().join("caller.gd"),
        "func total():\n\treturn widget_count() + 1\n",
    )
    .unwrap();

    let output = tsift_bin()
        .args(["index", dir.path().to_str().unwrap()])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "index stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    dir
}

#[test]
fn edit_intents_apply_renames_bash_zig_and_gdscript_symbols() {
    // `rename_symbol` used to exist for Rust, Python, and the JS-like family
    // only, because each of those hand-rolled its own substring scan. Reading
    // occurrences out of the grammar instead makes the kind language-general:
    // these three languages were already indexed, and registration is now a
    // per-language identifier-node set rather than another copy of the scan.
    let dir = indexed_tier_rename_fixture();
    for file in ["util.sh", "main.zig", "player.gd"] {
        let input = format!(
            r#"{{"intents":[{{"kind":"rename_symbol","symbol":"widget_count","file":"{file}","new_name":"gadget_count"}}]}}"#
        );
        let output = run_tsift_stdin(
            &[
                "--envelope",
                "edit-intents",
                "--path",
                dir.path().to_str().unwrap(),
                "--apply",
                "--json",
            ],
            &input,
        );
        assert!(
            output.status.success(),
            "{file} stderr: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        let json: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(json["report"]["applied_total"], 1, "{file}: {json}");
    }

    // Each position is asserted on its own. A test that only checked that the
    // file changed would pass while renaming the comment and the string.
    let bash = fs::read_to_string(dir.path().join("util.sh")).unwrap();
    assert!(bash.contains("gadget_count() {"), "{bash}");
    assert!(
        bash.contains("echo widget_count\n"),
        "an unquoted bash argument is data, not a name: {bash}"
    );
    assert!(bash.contains("label=\"widget_count\""), "{bash}");
    assert!(bash.contains("# widget_count comment"), "{bash}");

    let zig = fs::read_to_string(dir.path().join("main.zig")).unwrap();
    assert!(zig.contains("pub fn gadget_count() u32"), "{zig}");
    assert!(zig.contains("// widget_count comment"), "{zig}");
    assert!(zig.contains("\"widget_count\""), "{zig}");

    let gdscript = fs::read_to_string(dir.path().join("player.gd")).unwrap();
    assert!(gdscript.contains("func gadget_count():"), "{gdscript}");
    assert!(gdscript.contains("# widget_count comment"), "{gdscript}");
    assert!(gdscript.contains("\"widget_count\""), "{gdscript}");

    // The call-graph scoping added for the JS-like and Rust families is not
    // per-family either: a GDScript caller in another file is rewritten with no
    // GDScript-specific code, so the tree still resolves.
    let caller = fs::read_to_string(dir.path().join("caller.gd")).unwrap();
    assert!(
        caller.contains("return gadget_count() + 1"),
        "cross-file GDScript caller was left broken: {caller}"
    );
}

#[test]
fn edit_intents_refuses_structural_rewrite_for_an_indexed_language_with_no_grammar() {
    // Zig and GDScript are renamable because renaming needs a grammar and an
    // index, which they have; they are not structurally matchable, because this
    // build compiles no ast-grep grammar for them. This is the executor-level
    // "kind is not recognized" guard reaching the end-to-end path for the first
    // time — before there was no executor that recognized one kind and refused
    // another, so the guard was asserted in unit tests only.
    let dir = indexed_tier_rename_fixture();
    let input = r#"{"intents":[{"kind":"structural_rewrite","file":"main.zig","pattern":"foo($A)","replacement":"bar($A)"}]}"#;
    let output = run_tsift_stdin(
        &[
            "--envelope",
            "edit-intents",
            "--path",
            dir.path().to_str().unwrap(),
            "--json",
        ],
        input,
    );
    let json: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    let plan = &json["report"]["plans"][0];
    assert_eq!(plan["status"], "unsupported", "{json}");
    assert_eq!(plan["apply_supported"], false, "{json}");
    let message = plan["message"].as_str().unwrap();
    assert!(
        message.contains("Zig executor") && message.contains("not supported"),
        "refusal should name the executor: {message}"
    );
}

#[test]
fn edit_intents_refuses_rename_symbol_for_markdown_by_name() {
    // Markdown is indexed and stays deliberately out of the renamable set:
    // `rename_heading` is its kind. It is the one executor that resolves a
    // symbol and then refuses `rename_symbol`, so it proves the guard refuses
    // by name rather than letting the family split route the edit into another
    // language's rewriting rules.
    let dir = tempfile::tempdir().unwrap();
    fs::write(dir.path().join("README.md"), "# widgetCount\n\nbody\n").unwrap();
    let indexed = tsift_bin()
        .args(["index", dir.path().to_str().unwrap()])
        .output()
        .unwrap();
    assert!(indexed.status.success());

    let input = r#"{"intents":[{"kind":"rename_symbol","symbol":"widgetCount","file":"README.md","new_name":"gadgetCount"}]}"#;
    let output = run_tsift_stdin(
        &[
            "--envelope",
            "edit-intents",
            "--path",
            dir.path().to_str().unwrap(),
            "--json",
        ],
        input,
    );
    let json: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    let plan = &json["report"]["plans"][0];
    assert_eq!(plan["status"], "unsupported", "{json}");
    let message = plan["message"].as_str().unwrap();
    assert!(
        message.contains("Markdown executor") && message.contains("not supported"),
        "refusal should name the executor: {message}"
    );
    assert_eq!(
        fs::read_to_string(dir.path().join("README.md")).unwrap(),
        "# widgetCount\n\nbody\n",
        "a refused rename must not write"
    );
}

#[test]
fn edit_intents_refuses_a_structural_only_rename_at_the_index_layer() {
    // The plan for this phase expected the executor-level refusal of
    // `rename_symbol` for structural-only languages to become reachable here.
    // It does not: making every *indexed* language renamable leaves the
    // structural-only tier defined by having no `tsift-graph` binding at all,
    // so there is no symbol to resolve and the index layer refuses first. This
    // records where the refusal actually comes from, so a later change that
    // moves it is noticed instead of being read as a regression.
    // #goindex moved Go out of the structural-only tier, so this uses Java —
    // still ast-grep-only, with no `tsift-graph` binding.
    let dir = tempfile::tempdir().unwrap();
    fs::write(
        dir.path().join("Widget.java"),
        "class Widget { int widgetCount() { return 3; } }\n",
    )
    .unwrap();
    let indexed = tsift_bin()
        .args(["index", dir.path().to_str().unwrap()])
        .output()
        .unwrap();
    assert!(indexed.status.success());

    let input = r#"{"intents":[{"kind":"rename_symbol","symbol":"widgetCount","file":"Widget.java","new_name":"gadgetCount"}]}"#;
    let output = run_tsift_stdin(
        &[
            "--envelope",
            "edit-intents",
            "--path",
            dir.path().to_str().unwrap(),
            "--json",
        ],
        input,
    );
    assert!(!output.status.success(), "a Java rename must not be planned");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("no indexed symbol matched"),
        "expected the index layer to refuse first: {stderr}"
    );
    assert_eq!(
        fs::read_to_string(dir.path().join("Widget.java")).unwrap(),
        "class Widget { int widgetCount() { return 3; } }\n",
        "a refused rename must not write"
    );
}

// #goindex: the mirror image — a Go module is now indexed, searchable, and
// renamable, where before `search`, `explain`, and `graph` were blind to every
// Go symbol in the repo and `call_edges` stayed empty.
#[test]
fn go_sources_are_indexed_searchable_and_renamable() {
    let dir = tempfile::tempdir().unwrap();
    fs::write(dir.path().join("go.mod"), "module example.com/widget\n").unwrap();
    fs::write(
        dir.path().join("main.go"),
        "package main\n\nfunc widgetCount() int { return 3 }\n\nfunc main() { widgetCount() }\n",
    )
    .unwrap();
    let root = dir.path().to_str().unwrap();

    let indexed = tsift_bin().args(["index", root]).output().unwrap();
    assert!(
        indexed.status.success(),
        "index stderr: {}",
        String::from_utf8_lossy(&indexed.stderr)
    );

    let search = tsift_bin()
        .args(["search", "widgetCount", "--path", root, "--json"])
        .output()
        .unwrap();
    assert!(search.status.success());
    let json: serde_json::Value = serde_json::from_slice(&search.stdout).unwrap();
    assert!(
        json.to_string().contains("main.go"),
        "a Go symbol must be a search candidate: {json}"
    );

    let graph = tsift_bin()
        .args(["graph", "widgetCount", root, "--callers", "--json"])
        .output()
        .unwrap();
    assert!(graph.status.success());
    let graph_json: serde_json::Value = serde_json::from_slice(&graph.stdout).unwrap();
    assert!(
        graph_json.to_string().contains("main"),
        "Go call edges must be extracted: {graph_json}"
    );

    let input = r#"{"intents":[{"kind":"rename_symbol","symbol":"widgetCount","file":"main.go","new_name":"gadgetCount"}]}"#;
    let output = run_tsift_stdin(
        &["--envelope", "edit-intents", "--path", root, "--json", "--apply"],
        input,
    );
    assert!(
        output.status.success(),
        "a Go rename must be planned and applied: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let rewritten = fs::read_to_string(dir.path().join("main.go")).unwrap();
    assert!(
        rewritten.contains("gadgetCount") && !rewritten.contains("widgetCount"),
        "{rewritten}"
    );
}

#[test]
fn edit_intents_rename_leaves_a_same_named_field_and_local_alone() {
    // The grammar spells a Rust struct field and a method call the same way
    // (`field_identifier`), and a GDScript `func` and a local `var` the same
    // way (`name`). Without the resolved symbol kind the walk cannot tell them
    // apart, so a rename rewrote all of them. This drives the real planner, not
    // the occurrence walk, because the symbol kind has to survive the whole
    // path from the index to the rewrite for the narrowing to be reachable.
    let dir = tempfile::tempdir().unwrap();
    fs::write(
        dir.path().join("meter.rs"),
        "pub struct Meter { pub widget_count: usize }\n\
         pub fn widget_count() -> usize { 3 }\n\
         impl Meter {\n\
         \x20   pub fn widget_count(&self) -> usize { self.widget_count }\n\
         }\n\
         pub fn total(m: &Meter) -> usize { m.widget_count() + m.widget_count + widget_count() }\n",
    )
    .unwrap();
    fs::write(
        dir.path().join("player.gd"),
        "func widget_count():\n\tvar widget_count = 1\n\treturn 2\n\nfunc caller():\n\treturn widget_count()\n",
    )
    .unwrap();
    let indexed = tsift_bin()
        .args(["index", dir.path().to_str().unwrap()])
        .output()
        .unwrap();
    assert!(
        indexed.status.success(),
        "index stderr: {}",
        String::from_utf8_lossy(&indexed.stderr)
    );

    for file in ["meter.rs", "player.gd"] {
        let input = format!(
            r#"{{"intents":[{{"kind":"rename_symbol","symbol":"widget_count","file":"{file}","new_name":"gadget_count"}}]}}"#
        );
        let output = run_tsift_stdin(
            &[
                "--envelope",
                "edit-intents",
                "--path",
                dir.path().to_str().unwrap(),
                "--apply",
                "--json",
            ],
            &input,
        );
        assert!(
            output.status.success(),
            "{file} stderr: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        let json: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(json["report"]["applied_total"], 1, "{file}: {json}");
    }

    let rust = fs::read_to_string(dir.path().join("meter.rs")).unwrap();
    // Renamed: every callable position, including the method call, which the
    // narrowing must not drop.
    assert!(rust.contains("pub fn gadget_count() -> usize"), "{rust}");
    assert!(rust.contains("pub fn gadget_count(&self)"), "{rust}");
    assert!(rust.contains("m.gadget_count() +"), "{rust}");
    assert!(rust.contains("+ gadget_count()"), "{rust}");
    // Untouched: every field position.
    // rustfmt reflows the applied buffer, so the field declaration is asserted
    // on its own line rather than as the original one-liner.
    assert!(
        rust.contains("pub widget_count: usize,"),
        "the field declaration was renamed: {rust}"
    );
    assert!(
        !rust.contains("fn widget_count"),
        "a callable position was left behind: {rust}"
    );
    assert!(
        rust.contains("self.widget_count"),
        "a field read was renamed: {rust}"
    );
    assert!(
        rust.contains("+ m.widget_count +"),
        "a field read was renamed: {rust}"
    );

    let gdscript = fs::read_to_string(dir.path().join("player.gd")).unwrap();
    assert!(gdscript.contains("func gadget_count():"), "{gdscript}");
    assert!(gdscript.contains("return gadget_count()"), "{gdscript}");
    assert!(
        gdscript.contains("var widget_count = 1"),
        "the local var declaration was renamed: {gdscript}"
    );
}

#[test]
fn edit_intents_rename_leaves_javascript_properties_alone() {
    // `property_identifier` covers an object-literal key, a class method, and a
    // member access, none of which is the module-level binding the planner
    // resolved — `Lang::symbol_query` indexes only top-level declarations. The
    // object-literal shorthand is the one token that is both a property name
    // and a read of the binding, so it expands rather than being overwritten.
    let dir = tempfile::tempdir().unwrap();
    fs::write(
        dir.path().join("app.js"),
        "function beta(v) { return v; }\n\
         const keyed = { beta: 1 };\n\
         const shorthand = { beta };\n\
         class Panel { beta() { return 2; } }\n\
         const total = keyed.beta + beta(3);\n\
         module.exports = { keyed, shorthand, total };\n",
    )
    .unwrap();
    let indexed = tsift_bin()
        .args(["index", dir.path().to_str().unwrap()])
        .output()
        .unwrap();
    assert!(
        indexed.status.success(),
        "index stderr: {}",
        String::from_utf8_lossy(&indexed.stderr)
    );

    let input = r#"{"intents":[{"kind":"rename_symbol","symbol":"beta","file":"app.js","new_name":"gamma"}]}"#;
    let output = run_tsift_stdin(
        &[
            "--envelope",
            "edit-intents",
            "--path",
            dir.path().to_str().unwrap(),
            "--apply",
            "--json",
        ],
        input,
    );
    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let json: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(json["report"]["applied_total"], 1, "{json}");

    let source = fs::read_to_string(dir.path().join("app.js")).unwrap();
    assert!(source.contains("function gamma(v)"), "{source}");
    assert!(source.contains("gamma(3)"), "{source}");
    // The shorthand keeps its property name and follows the rename.
    assert!(
        source.contains("{ beta: gamma }"),
        "shorthand was not expanded: {source}"
    );
    // Every other property position is untouched.
    assert!(source.contains("{ beta: 1 }"), "object key: {source}");
    assert!(
        source.contains("beta() { return 2; }"),
        "class method: {source}"
    );
    assert!(source.contains("keyed.beta"), "member read: {source}");
}
