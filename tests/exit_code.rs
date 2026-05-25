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
    Command::new(env!("CARGO_BIN_EXE_tsift"))
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
    let spec = fs::read_to_string(root.join("SPEC.md")).unwrap();
    let readme = fs::read_to_string(root.join("README.md")).unwrap();

    assert!(
        workflow.contains("cargo publish --locked --dry-run"),
        "release verification should prove the crate package is publishable"
    );
    assert!(
        workflow.contains("vars.TSIFT_ENABLE_CRATES_PUBLISH == 'true'"),
        "publish job should remain opt-in through the repo variable"
    );
    assert!(
        workflow.contains("CARGO_REGISTRY_TOKEN: ${{ secrets.CARGO_REGISTRY_TOKEN }}"),
        "publish job should read the crates.io token from the repo secret"
    );
    assert!(
        spec.contains("TSIFT_ENABLE_CRATES_PUBLISH=true")
            && spec.contains("CARGO_REGISTRY_TOKEN")
            && spec.contains("cargo publish --locked --dry-run"),
        "release spec should document the publish gate"
    );
    assert!(
        readme.contains("TSIFT_ENABLE_CRATES_PUBLISH=true")
            && readme.contains("CARGO_REGISTRY_TOKEN"),
        "README should document the repo variable and secret"
    );
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

fn write_missing_summary_api_key_config(dir: &Path) {
    fs::create_dir_all(dir.join(".tsift")).unwrap();
    fs::write(
        dir.join(".tsift/config.toml"),
        "[summarize]\napi_key_env = \"TSIFT_TEST_NONEXISTENT_KEY\"\n",
    )
    .unwrap();
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
            names.contains(&"delta".to_string())
                && names.contains(&"epsilon".to_string())
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

    let mut helper_handles: Vec<String> = Vec::new();
    for community in json["communities"].as_array().unwrap() {
        for member in community["members"].as_array().unwrap() {
            if member["name"].as_str() == Some("helper")
                && let Some(handle) = member.get("tagpath_handle").and_then(|h| h.as_str())
            {
                helper_handles.push(handle.to_string());
            }
        }
    }
    assert!(
        !helper_handles.is_empty(),
        "expected `helper` member to carry a tagpath_handle after the name-collision fix: {json}"
    );
    for handle in &helper_handles {
        assert!(handle.starts_with("mem:"), "{handle}");
    }
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
    assert_eq!(json["report"]["preview"].as_array().unwrap().len(), 8);
    assert!(
        json["report"]["preview"][0]["text"]
            .as_str()
            .unwrap()
            .contains("fn main")
    );
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
            .any(|cmd| cmd.as_str().unwrap().contains("tsift source-read"))
    );

    let symbols = json["report"]["symbols"].as_array().unwrap();
    assert!(
        symbols.iter().any(|symbol| {
            symbol["handle"].as_str().unwrap().starts_with("ssym-")
                && symbol["name"] == "main"
                && symbol["expand"]
                    .as_str()
                    .unwrap()
                    .contains("tsift --envelope explain")
        }),
        "expected main symbol ref: {json}"
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
fn workspace_graph_queries_require_scope_without_shared_root_index() {
    let dir = indexed_workspace_cli_fixture();
    let root = dir.path().to_str().unwrap();
    let cases = [
        ("graph", vec!["graph", "alpha_main", root, "--json"]),
        ("communities", vec!["communities", root, "--json"]),
        (
            "path",
            vec!["path", "alpha_main", "alpha_helper", root, "--json"],
        ),
        ("explain", vec!["explain", "alpha_main", root, "--json"]),
    ];

    for (label, args) in cases {
        let output = tsift_bin().args(args).output().unwrap();
        assert!(
            !output.status.success(),
            "{label} should fail closed without an explicit workspace scope"
        );
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(stderr.contains("require `--scope <scope>`"), "{stderr}");
        assert!(stderr.contains("Available scopes: alpha, beta"), "{stderr}");
        assert!(stderr.contains("Indexed scopes: alpha, beta"), "{stderr}");
        assert!(!stderr.contains("no index found at"), "{stderr}");
    }
}

#[test]
fn workspace_search_requires_explicit_scope_or_federated_without_shared_root_index() {
    let dir = indexed_workspace_cli_fixture();
    let root = dir.path().to_str().unwrap();

    let output = tsift_bin()
        .args(["search", "helper", "--path", root, "--json"])
        .output()
        .unwrap();

    assert!(
        !output.status.success(),
        "workspace search should fail closed without an explicit target"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("requires `--scope <scope>` or `--federated`"),
        "{stderr}"
    );
    assert!(stderr.contains("Available scopes: alpha, beta"), "{stderr}");
    assert!(stderr.contains("Indexed scopes: alpha, beta"), "{stderr}");
    assert!(!dir.path().join(".tsift/index.db").exists());
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
    write_missing_summary_api_key_config(project.path());

    let caller = tempfile::tempdir().unwrap();
    fs::create_dir_all(caller.path().join("src")).unwrap();

    let output = tsift_bin()
        .current_dir(caller.path())
        .env_remove("ANTHROPIC_API_KEY")
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
    assert!(stdout.contains("errors:1"), "stdout was: {stdout}");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("src/main.rs"), "stderr was: {stderr}");
}

#[test]
fn summarize_extract_uses_nested_path_as_relative_extract_anchor() {
    let project = tempfile::tempdir().unwrap();
    fs::create_dir_all(project.path().join("src")).unwrap();
    fs::create_dir_all(project.path().join("src/nested")).unwrap();
    fs::write(project.path().join("src/main.rs"), "fn root_helper() {}\n").unwrap();
    fs::write(
        project.path().join("src/nested/main.rs"),
        "fn nested_helper() {}\n",
    )
    .unwrap();
    write_missing_summary_api_key_config(project.path());

    let nested = project.path().join("src/nested");
    let output = tsift_bin()
        .current_dir(project.path())
        .env_remove("ANTHROPIC_API_KEY")
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
    assert!(stdout.contains("errors:1"), "stdout was: {stdout}");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("src/nested/"), "stderr was: {stderr}");
    assert!(
        !stderr.contains("error: src/main.rs"),
        "stderr was: {stderr}"
    );
    assert!(!nested.join(".tsift/summaries.db").exists());
    assert!(project.path().join(".tsift/summaries.db").exists());
}

#[test]
fn summarize_diff_extract_includes_untracked_files() {
    let dir = tempfile::tempdir().unwrap();
    fs::write(dir.path().join("README.md"), "# repo\n").unwrap();
    init_git_repo(dir.path());

    fs::create_dir_all(dir.path().join("src")).unwrap();
    fs::write(dir.path().join("src/new.rs"), "fn alpha_helper() {}\n").unwrap();
    write_missing_summary_api_key_config(dir.path());

    let output = tsift_bin()
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
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("missing API key"), "stderr was: {stderr}");
    assert!(stderr.contains("src/new.rs"), "stderr was: {stderr}");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        !stdout.contains("No files to extract."),
        "stdout was: {stdout}"
    );
    assert!(stdout.contains("errors:1"), "stdout was: {stdout}");
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
    write_missing_summary_api_key_config(dir.path());

    let output = tsift_bin()
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
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("missing API key"), "stderr was: {stderr}");
    assert!(stderr.contains("src/new.rs"), "stderr was: {stderr}");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        !stdout.contains("No files to extract."),
        "stdout was: {stdout}"
    );
    assert!(stdout.contains("errors:1"), "stdout was: {stdout}");
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
    write_missing_summary_api_key_config(dir.path());

    let output = tsift_bin()
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
    assert!(stdout.contains("errors:1"), "stdout was: {stdout}");

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("src/lib.rs"), "stderr was: {stderr}");
    assert!(
        !stderr.contains("src/../src/lib.rs"),
        "stderr was: {stderr}"
    );
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
    assert!(
        dir.path().join(".tsift/search-cache").exists(),
        "timeout=0 should still populate the stable search cache dir"
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
        r#"{"timestamp":"2026-05-05T00:00:01Z","type":"event_msg","payload":{"type":"token_count","info":{"total_token_usage":{"input_tokens":24000,"cached_input_tokens":23000,"output_tokens":300,"reasoning_output_tokens":100,"total_tokens":24300}}}}"#,
        "\n",
        r#"{"timestamp":"2026-05-05T00:00:04Z","type":"event_msg","payload":{"type":"token_count","info":{"total_token_usage":{"input_tokens":50000,"cached_input_tokens":48000,"output_tokens":650,"reasoning_output_tokens":180,"total_tokens":50650}}}}"#,
        "\n",
        r#"{"timestamp":"2026-05-05T00:00:05Z","type":"event_msg","payload":{"type":"token_count","info":{"total_token_usage":{"input_tokens":50000,"cached_input_tokens":48000,"output_tokens":650,"reasoning_output_tokens":180,"total_tokens":50650}}}}"#,
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
    assert!(
        json["guardrails"]
            .as_array()
            .unwrap()
            .iter()
            .any(|guardrail| guardrail["kind"] == "cache_resend")
    );
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
        "---\nagent_doc_session: tsift-v0.1\n---\n\n## Exchange\n❯ do [#ts1b]. spec-test-build-install-commit-push\n",
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

    assert_eq!(output.status.code(), Some(7));
    let json: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(json["runner"], "cargo");
    assert_eq!(json["failures"], 1);
    assert_eq!(json["failure_groups"][0]["path"], "src/lib.rs");
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
            "__digest-runner",
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
            "__digest-runner",
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

    let output = tsift_bin()
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
