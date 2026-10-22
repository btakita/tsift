//! Integration tests for file-backed decision manifests.

use std::fs;
use std::path::Path;
use std::process::{Command, Output};

fn run(args: &[&str], cwd: &Path) -> Output {
    Command::new(env!("CARGO_BIN_EXE_tsift-cli"))
        .args(args)
        .current_dir(cwd)
        .output()
        .unwrap()
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

fn write_fixture(root: &Path) {
    fs::create_dir_all(root.join("docs")).unwrap();
    fs::write(root.join("main.rs"), "pub fn choose() {}\n").unwrap();
    fs::write(root.join("docs/decision.md"), "# Decision context\n").unwrap();
    fs::write(
        root.join("decisions.md"),
        r#"# Decision manifest

| id | status | date | decision | source | anchors | supersedes | related |
|---|---|---|---|---|---|---|---|
| dec-0001 | superseded | 2026-01-02 | Keep the old implementation | docs/decision.md | file:main.rs | - | - |
| dec-0002 | accepted | 2026-02-03 | Use the new implementation | docs/decision.md#new | file:main.rs | dec-0001 | dec-0001 |
"#,
    )
    .unwrap();
}

#[test]
fn lint_and_import_project_stable_decision_nodes() {
    let dir = tempfile::tempdir().unwrap();
    write_fixture(dir.path());

    let lint: serde_json::Value = serde_json::from_str(&run_ok(
        &["decision", "lint", "decisions.md", "--path", ".", "--json"],
        dir.path(),
    ))
    .unwrap();
    assert_eq!(lint["valid"], true);
    assert_eq!(lint["decision_count"], 2);
    assert_eq!(lint["accepted_count"], 1);

    let imported: serde_json::Value = serde_json::from_str(&run_ok(
        &[
            "decision",
            "import",
            "decisions.md",
            "--path",
            ".",
            "--json",
        ],
        dir.path(),
    ))
    .unwrap();
    assert_eq!(imported["decisions_imported"], 2);
    assert_eq!(imported["accepted_decisions"], 1);
    assert!(dir.path().join(".tsift/findings.db").is_file());

    // Import is idempotent: the same explicit IDs are upserted, not duplicated.
    run_ok(
        &[
            "decision",
            "import",
            "decisions.md",
            "--path",
            ".",
            "--json",
        ],
        dir.path(),
    );
    let listed: serde_json::Value = serde_json::from_str(&run_ok(
        &[
            "finding",
            "list",
            ".",
            "--kind",
            "decision",
            "--include-stale",
            "--json",
        ],
        dir.path(),
    ))
    .unwrap();
    assert_eq!(listed["total"], 2);
    let findings = listed["findings"].as_array().unwrap();
    assert!(findings.iter().any(|item| {
        item["id"] == "decision:dec-0002"
            && item["status"] == "trusted"
            && item["title"] == "Use the new implementation"
    }));
    assert!(
        findings
            .iter()
            .any(|item| { item["id"] == "decision:dec-0001" && item["status"] == "draft" })
    );
}

#[test]
fn lint_rejects_missing_relationship_targets() {
    let dir = tempfile::tempdir().unwrap();
    fs::write(
        dir.path().join("decisions.md"),
        r#"| id | status | date | decision | source | anchors | supersedes | related |
|---|---|---|---|---|---|---|---|
| dec-0001 | accepted | 2026-02-03 | Use the new implementation | - | - | dec-9999 | - |
"#,
    )
    .unwrap();

    let output = run(
        &["decision", "lint", "decisions.md", "--path", "."],
        dir.path(),
    );
    assert!(!output.status.success());
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("references missing decision `dec-9999`")
    );
}

#[test]
fn lint_rejects_invalid_lifecycle_and_escaping_paths() {
    let dir = tempfile::tempdir().unwrap();
    fs::write(
        dir.path().join("decisions.md"),
        r#"| id | status | date | decision | source | anchors | supersedes | related |
|---|---|---|---|---|---|---|---|
| dec-0001 | proposed | 2026-02-03 | Proposed choice | ../outside.md | - | - | - |
"#,
    )
    .unwrap();

    let output = run(
        &["decision", "lint", "decisions.md", "--path", "."],
        dir.path(),
    );
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("must not escape the repository"));
}

#[test]
fn import_fails_closed_for_an_unresolved_code_symbol_anchor() {
    let dir = tempfile::tempdir().unwrap();
    fs::write(
        dir.path().join("decisions.md"),
        r#"| id | status | date | decision | source | anchors | supersedes | related |
|---|---|---|---|---|---|---|---|
| dec-0001 | accepted | 2026-02-03 | Use the indexed store | - | code_symbol:MissingStore | - | - |
"#,
    )
    .unwrap();

    run_ok(
        &["decision", "lint", "decisions.md", "--path", "."],
        dir.path(),
    );
    let output = run(
        &["decision", "import", "decisions.md", "--path", "."],
        dir.path(),
    );
    assert!(!output.status.success());
    assert!(
        String::from_utf8_lossy(&output.stderr)
            .contains("is not present in the current index; run `tsift index` and retry")
    );
}
