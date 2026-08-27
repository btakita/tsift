//! End-to-end coverage for `tsift ast-grep` (structural search + rewrite).

use std::fs;
use std::path::Path;
use std::process::Command;

fn tsift_bin() -> Command {
    Command::new(env!("CARGO_BIN_EXE_tsift-cli"))
}

fn run(args: &[&str]) -> std::process::Output {
    tsift_bin().args(args).output().unwrap()
}

fn fixture() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    fs::write(dir.path().join("a.rs"), "fn a() {\n    foo(1);\n}\n").unwrap();
    fs::write(dir.path().join("b.rs"), "fn b() {\n    foo(2);\n    foo(3);\n}\n").unwrap();
    fs::write(dir.path().join("notes.txt"), "foo(4);\n").unwrap();
    dir
}

fn root(dir: &tempfile::TempDir) -> &str {
    dir.path().to_str().unwrap()
}

fn read(dir: &tempfile::TempDir, name: &str) -> String {
    fs::read_to_string(dir.path().join(name)).unwrap()
}

#[test]
fn search_reports_every_structural_match_as_json() {
    let dir = fixture();
    let out = run(&["ast-grep", "search", "foo($A)", "--path", root(&dir), "--json"]);
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let report: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(report["match_count"], 3);
    assert_eq!(report["files"].as_array().unwrap().len(), 2);
    assert_eq!(report["truncated"], false);
    let first = &report["files"][0]["matches"][0];
    assert_eq!(first["captures"]["A"], "1");
    assert_eq!(first["start_line"], 2);
    assert_eq!(first["start_column"], 5);
}

#[test]
fn search_text_output_cites_file_line_column() {
    let dir = fixture();
    let out = run(&["ast-grep", "search", "foo($A)", "--path", root(&dir)]);
    assert!(out.status.success());
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("a.rs:2:5: foo(1)"), "got: {stdout}");
    assert!(stdout.contains("3 match(es)"), "got: {stdout}");
}

#[test]
fn rewrite_defaults_to_preview_and_leaves_the_tree_untouched() {
    let dir = fixture();
    let before = read(&dir, "b.rs");
    let out = run(&[
        "ast-grep",
        "rewrite",
        "foo($A)",
        "bar($A)",
        "--path",
        root(&dir),
        "--json",
    ]);
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let report: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(report["replacements"], 3);
    assert_eq!(report["applied"], false);
    assert_eq!(read(&dir, "b.rs"), before, "preview must not write");
}

#[test]
fn rewrite_apply_writes_all_matching_files() {
    let dir = fixture();
    let out = run(&[
        "ast-grep",
        "rewrite",
        "foo($A)",
        "bar($A)",
        "--path",
        root(&dir),
        "--apply",
        "--json",
    ]);
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    assert!(read(&dir, "a.rs").contains("bar(1)"));
    assert!(read(&dir, "b.rs").contains("bar(2)"));
    assert!(read(&dir, "b.rs").contains("bar(3)"));
    assert_eq!(
        read(&dir, "notes.txt"),
        "foo(4);\n",
        "non-source files must be untouched"
    );
}

#[test]
fn apply_under_a_preview_budget_is_refused() {
    // A capped scan that still wrote would land a partial codemod and report it
    // as complete.
    let dir = fixture();
    let out = run(&[
        "ast-grep",
        "rewrite",
        "foo($A)",
        "bar($A)",
        "--path",
        root(&dir),
        "--apply",
        "--max-items",
        "1",
    ]);
    assert!(!out.status.success());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("--apply cannot run under a preview budget"), "got: {stderr}");
    assert!(read(&dir, "a.rs").contains("foo(1)"), "nothing may be written");
}

#[test]
fn budgeted_search_marks_itself_truncated() {
    let dir = fixture();
    let out = run(&[
        "ast-grep",
        "search",
        "foo($A)",
        "--path",
        root(&dir),
        "--json",
        "--max-items",
        "1",
    ]);
    assert!(out.status.success());
    let report: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(report["truncated"], true);
}

#[test]
fn unsupported_language_is_rejected() {
    let dir = fixture();
    let out = run(&[
        "ast-grep",
        "search",
        "foo($A)",
        "--path",
        root(&dir),
        "--lang",
        "cobol",
    ]);
    assert!(!out.status.success());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("unsupported --lang 'cobol'"), "got: {stderr}");
}

#[test]
fn languages_lists_the_compiled_grammars() {
    let out = run(&["ast-grep", "languages", "--json"]);
    assert!(out.status.success());
    let report: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    let langs = report["languages"].as_array().unwrap();
    assert!(langs.iter().any(|l| l == "rust"), "got: {langs:?}");
}

#[test]
fn envelope_view_carries_a_summary_and_metrics() {
    let dir = fixture();
    let out = run(&[
        "--envelope",
        "ast-grep",
        "search",
        "foo($A)",
        "--path",
        root(&dir),
    ]);
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let envelope: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(envelope["tool"], "ast-grep");
    assert_eq!(envelope["view"], "search");
    assert!(
        envelope["summary"]["text"]
            .as_str()
            .unwrap()
            .contains("structural search")
    );
}

#[test]
fn gitignored_files_are_skipped_unless_no_ignore() {
    let dir = tempfile::tempdir().unwrap();
    // `ignore` only applies gitignore rules inside a git repo.
    assert!(
        Command::new("git")
            .args(["init", "--quiet"])
            .current_dir(dir.path())
            .status()
            .unwrap()
            .success()
    );
    fs::write(dir.path().join(".gitignore"), "vendor/\n").unwrap();
    fs::create_dir(dir.path().join("vendor")).unwrap();
    fs::write(dir.path().join("vendor/v.rs"), "fn v() { foo(9); }\n").unwrap();
    fs::write(dir.path().join("keep.rs"), "fn k() { foo(1); }\n").unwrap();

    let root_arg = dir.path().to_str().unwrap();
    let ignored = run(&["ast-grep", "search", "foo($A)", "--path", root_arg, "--json"]);
    let report: serde_json::Value = serde_json::from_slice(&ignored.stdout).unwrap();
    assert_eq!(report["match_count"], 1, "vendor/ must be excluded");

    let unignored = run(&[
        "ast-grep",
        "search",
        "foo($A)",
        "--path",
        root_arg,
        "--json",
        "--no-ignore",
    ]);
    let report: serde_json::Value = serde_json::from_slice(&unignored.stdout).unwrap();
    assert_eq!(report["match_count"], 2, "--no-ignore must include vendor/");
}

#[test]
fn explicit_file_path_is_scanned_directly() {
    let dir = fixture();
    let file = dir.path().join("b.rs");
    let out = run(&[
        "ast-grep",
        "search",
        "foo($A)",
        "--path",
        file.to_str().unwrap(),
        "--json",
    ]);
    assert!(out.status.success());
    let report: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(report["match_count"], 2);
}

#[test]
fn missing_path_exits_nonzero() {
    let out = run(&[
        "ast-grep",
        "search",
        "foo($A)",
        "--path",
        "/definitely/not/here",
    ]);
    assert!(!out.status.success());
    assert!(!Path::new("/definitely/not/here").exists());
}
