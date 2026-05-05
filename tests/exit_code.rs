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
    assert!(
        communities.iter().any(|community| {
            let members = community["members"].as_array().unwrap();
            members.iter().any(|m| m == "alpha")
                && members.iter().any(|m| m == "beta")
                && members.iter().any(|m| m == "gamma")
        }),
        "expected alpha/beta/gamma community: {json}"
    );
    assert!(
        communities.iter().any(|community| {
            let members = community["members"].as_array().unwrap();
            members.iter().any(|m| m == "delta") && members.iter().any(|m| m == "epsilon")
        }),
        "expected delta/epsilon community: {json}"
    );
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
    assert_eq!(
        json["path"],
        serde_json::json!(["main", "bridge", "shared", "helper"])
    );
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
    assert!(community_members.iter().any(|member| member == "alpha"));
    assert!(community_members.iter().any(|member| member == "beta"));
    assert!(community_members.iter().any(|member| member == "gamma"));
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
fn search_autoindex_fails_fast_when_writer_lock_exists() {
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

    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("another tsift index writer is already active"));
    assert!(stderr.contains("lock diagnostics:"));
    assert!(stderr.contains("lock: live pid:"));
    assert!(stderr.contains("journal: absent"));
    assert!(stderr.contains("next: wait for the active tsift writer"));
    assert!(stderr.contains("search --autoindex"));
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
    assert!(stdout.contains("#1 [High] notes.md (hits: 3"), "{stdout}");
    assert!(stdout.contains("(+1 more hits in file)"), "{stdout}");
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
