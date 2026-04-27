use std::fs;
use std::process::Command;

fn tsift_bin() -> Command {
    Command::new(env!("CARGO_BIN_EXE_tsift"))
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
        .args(["index", "--check", "--exit-code", dir.path().to_str().unwrap()])
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
        .args(["index", "--check", "--exit-code", dir.path().to_str().unwrap()])
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
    assert!(status.success(), "expected exit 0 when --exit-code not specified");
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
    fs::write(dir.path().join("main.rs"), "fn main() { println!(\"hi\"); }").unwrap();

    let status = tsift_bin()
        .args(["index", "--check", "--exit-code", dir.path().to_str().unwrap()])
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
        .args(["index", "--check", "--exit-code", dir.path().to_str().unwrap()])
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
        .args(["index", "--check", "--exit-code", dir.path().to_str().unwrap()])
        .status()
        .unwrap();
    assert!(!status.success(), "expected exit 1 when no index exists (all files are new)");
}
