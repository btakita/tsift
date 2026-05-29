//! Tagpath consumer adapter for tsift.
//!
//! Wraps `tagpath::index` so tsift can use tagpath's `.naming/index.json`
//! snapshot as a stable symbol-graph adapter. The adapter loads a fresh
//! tagpath index when one is present at the project root and surfaces
//! stable `fam:`/`mem:` handles for citation in tsift envelopes.
//!
//! Reference contract: `src/tagpath/SPEC.md` §15 (Consumer contract).
//!
//! ## Lifecycle
//!
//! - [`try_load`] looks for `.naming/index.json` at the given project root,
//!   runs `tagpath::index::check` for freshness, and returns one of
//!   [`LoadResult::Loaded`], [`LoadResult::Stale`], or [`LoadResult::Missing`].
//! - Consumers can join tsift search hits back to tagpath handles via
//!   [`TagpathAdapter::handle_for_member`] or look up a known handle via
//!   [`TagpathAdapter::lookup_handle`].
//! - The adapter does not own a long-lived watch subscription; callers
//!   that want freshness can drop the adapter and call [`try_load`] again,
//!   or spawn `tagpath index --update` out-of-band.

use std::path::{Path, PathBuf};

use tagpath::index::{self as tp_index, Family, FamilyMember, Index, StaleReason};

/// Loaded tagpath index plus enough context to answer handle lookups.
pub struct TagpathAdapter {
    pub project_root: PathBuf,
    pub index: Index,
}

/// Result of [`try_load`].
pub enum LoadResult {
    /// Index present, fresh, and loaded.
    Loaded(TagpathAdapter),
    /// Index present but stale (schema mismatch, config drift, source delta).
    /// `reason` is a human-readable summary of the first stale reason.
    Stale {
        reason: String,
        project_root: PathBuf,
    },
    /// No `.naming.toml` / `.naming/index.json` at this root — fall back to
    /// tsift's native extraction.
    Missing,
}

/// Resolution of a `fam:`/`mem:` handle back to the underlying family.
#[derive(Debug, Clone)]
pub enum HandleResolution<'a> {
    Family(&'a Family),
    Member {
        family: &'a Family,
        member: &'a FamilyMember,
    },
}

/// Try to load a tagpath index at `project_root`.
///
/// - Returns [`LoadResult::Missing`] when there is no `.naming.toml`
///   anywhere above the path, or no `.naming/index.json` snapshot yet.
/// - Returns [`LoadResult::Stale`] when an index is present but
///   `tagpath::index::check` reports any non-`SchemaChanged` stale reason.
///   `SchemaChanged` (silent migration) is treated as a hard stale here
///   because we do not own a rebuild path; the caller is expected to run
///   `tagpath index --update` and retry.
/// - Returns [`LoadResult::Loaded`] when the on-disk index is fresh.
pub fn try_load(project_root: &Path) -> LoadResult {
    // Resolve to a real tagpath project root (walks up for `.naming.toml`).
    let root = match tp_index::find_project_root(project_root) {
        Some(p) => p,
        None => return LoadResult::Missing,
    };
    let idx_path = tp_index::index_path(&root);
    if !idx_path.exists() {
        return LoadResult::Missing;
    }
    match tp_index::check(&root) {
        Ok(report) if report.fresh => match tp_index::read(&report.index_path) {
            Ok(index) => LoadResult::Loaded(TagpathAdapter {
                project_root: root,
                index,
            }),
            Err(message) => LoadResult::Stale {
                reason: format!("index_unreadable: {message}"),
                project_root: root,
            },
        },
        Ok(report) => {
            let reason = report
                .stale_reasons
                .first()
                .map(stale_reason_to_string)
                .unwrap_or_else(|| "stale".to_string());
            LoadResult::Stale {
                reason,
                project_root: root,
            }
        }
        Err(message) => LoadResult::Stale {
            reason: format!("check_failed: {message}"),
            project_root: root,
        },
    }
}

fn stale_reason_to_string(reason: &StaleReason) -> String {
    match reason {
        StaleReason::IndexMissing => "index_missing".to_string(),
        StaleReason::IndexUnreadable { message } => format!("index_unreadable: {message}"),
        StaleReason::SchemaVersion { found, expected } => {
            format!("schema_version: found={found} expected={expected}")
        }
        StaleReason::SchemaChanged { found, expected } => {
            format!("schema_changed: found={found} expected={expected}")
        }
        StaleReason::ConfigChanged => "config_changed".to_string(),
        StaleReason::ToolVersion { found, expected } => {
            format!("tool_version: found={found} expected={expected}")
        }
        StaleReason::SourceAdded { path } => format!("source_added: {path}"),
        StaleReason::SourceRemoved { path } => format!("source_removed: {path}"),
        StaleReason::SourceModified { path } => format!("source_modified: {path}"),
    }
}

impl TagpathAdapter {
    /// Find the family that owns a given source file path. Path is
    /// interpreted relative to the tagpath project root.
    pub fn family_for_member(&self, path: &Path) -> Option<&Family> {
        let rel = relative_to_root(path, &self.project_root);
        self.index
            .families
            .iter()
            .find(|f| f.members.iter().any(|m| m.path == rel))
    }

    /// Look up the stable `mem:` handle for a (path, member-name) pair.
    pub fn handle_for_member(&self, path: &Path, name: &str) -> Option<String> {
        let rel = relative_to_root(path, &self.project_root);
        for fam in &self.index.families {
            for m in &fam.members {
                if m.path == rel && m.name == name {
                    return Some(m.handle.clone());
                }
            }
        }
        None
    }

    /// Resolve a `fam:` or `mem:` handle back to its in-index target.
    pub fn lookup_handle(&self, handle: &str) -> Option<HandleResolution<'_>> {
        if let Some(_rest) = handle.strip_prefix("fam:") {
            return self
                .index
                .families
                .iter()
                .find(|f| f.handle == handle)
                .map(HandleResolution::Family);
        }
        if let Some(_rest) = handle.strip_prefix("mem:") {
            for fam in &self.index.families {
                if let Some(m) = fam.members.iter().find(|m| m.handle == handle) {
                    return Some(HandleResolution::Member {
                        family: fam,
                        member: m,
                    });
                }
            }
        }
        None
    }
}

fn relative_to_root(path: &Path, root: &Path) -> String {
    // Try strip_prefix first against the literal and canonicalized root.
    if let Ok(rel) = path.strip_prefix(root) {
        return rel.to_string_lossy().replace('\\', "/");
    }
    if let (Ok(can_path), Ok(can_root)) = (path.canonicalize(), root.canonicalize())
        && let Ok(rel) = can_path.strip_prefix(&can_root)
    {
        return rel.to_string_lossy().replace('\\', "/");
    }
    path.to_string_lossy().replace('\\', "/")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::tempdir;

    fn write_minimal_project(root: &Path) {
        fs::write(
            root.join(".naming.toml"),
            r#"version = 1
name = "tsift-adapter-test"

convention = "snake_case"
immutable = true
singular = true

[vectors]
join = "_"
namespace = "__"

[externals]
preserve_casing = true
join_with = "_"

[tags]
open = true
"#,
        )
        .unwrap();
        fs::create_dir_all(root.join("src")).unwrap();
        fs::write(
            root.join("src/lib.rs"),
            "fn create_user() {}\nfn delete_user() {}\n",
        )
        .unwrap();
    }

    #[test]
    fn try_load_returns_missing_when_no_naming_toml() {
        let dir = tempdir().unwrap();
        match try_load(dir.path()) {
            LoadResult::Missing => {}
            _ => panic!("expected Missing"),
        }
    }

    #[test]
    fn try_load_returns_missing_when_index_absent() {
        let dir = tempdir().unwrap();
        write_minimal_project(dir.path());
        match try_load(dir.path()) {
            LoadResult::Missing => {}
            other => panic!(
                "expected Missing when index file is absent; got {:?}",
                debug_load(&other)
            ),
        }
    }

    #[test]
    fn try_load_returns_loaded_when_index_fresh() {
        let dir = tempdir().unwrap();
        write_minimal_project(dir.path());
        // Build the index using the tagpath library directly.
        let opts = tp_index::BuildOptions {
            project_root: dir.path().to_path_buf(),
        };
        let index = tp_index::build(&opts).expect("build index");
        let idx_path = tp_index::index_path(dir.path());
        fs::create_dir_all(idx_path.parent().unwrap()).unwrap();
        fs::write(&idx_path, serde_json::to_string(&index).unwrap()).unwrap();

        match try_load(dir.path()) {
            LoadResult::Loaded(adapter) => {
                assert!(!adapter.index.families.is_empty(), "expected families");
            }
            other => panic!("expected Loaded; got {:?}", debug_load(&other)),
        }
    }

    #[test]
    fn try_load_returns_stale_on_source_drift() {
        let dir = tempdir().unwrap();
        write_minimal_project(dir.path());
        let opts = tp_index::BuildOptions {
            project_root: dir.path().to_path_buf(),
        };
        let index = tp_index::build(&opts).expect("build");
        let idx_path = tp_index::index_path(dir.path());
        fs::create_dir_all(idx_path.parent().unwrap()).unwrap();
        fs::write(&idx_path, serde_json::to_string(&index).unwrap()).unwrap();

        // Mutate source so the on-disk index no longer matches.
        fs::write(
            dir.path().join("src/lib.rs"),
            "fn create_user() {}\nfn delete_user() {}\nfn fetch_user() {}\n",
        )
        .unwrap();

        match try_load(dir.path()) {
            LoadResult::Stale { reason, .. } => {
                assert!(
                    reason.contains("source_modified") || reason.contains("source_added"),
                    "unexpected stale reason: {reason}"
                );
            }
            other => panic!("expected Stale; got {:?}", debug_load(&other)),
        }
    }

    #[test]
    fn family_for_member_finds_known_path() {
        let dir = tempdir().unwrap();
        write_minimal_project(dir.path());
        let opts = tp_index::BuildOptions {
            project_root: dir.path().to_path_buf(),
        };
        let index = tp_index::build(&opts).unwrap();
        let idx_path = tp_index::index_path(dir.path());
        fs::create_dir_all(idx_path.parent().unwrap()).unwrap();
        fs::write(&idx_path, serde_json::to_string(&index).unwrap()).unwrap();

        let adapter = match try_load(dir.path()) {
            LoadResult::Loaded(a) => a,
            _ => panic!("expected Loaded"),
        };

        let family = adapter
            .family_for_member(&dir.path().join("src/lib.rs"))
            .expect("family for src/lib.rs");
        assert!(family.handle.starts_with("fam:"));
    }

    #[test]
    fn handle_for_member_returns_mem_handle() {
        let dir = tempdir().unwrap();
        write_minimal_project(dir.path());
        let opts = tp_index::BuildOptions {
            project_root: dir.path().to_path_buf(),
        };
        let index = tp_index::build(&opts).unwrap();
        let idx_path = tp_index::index_path(dir.path());
        fs::create_dir_all(idx_path.parent().unwrap()).unwrap();
        fs::write(&idx_path, serde_json::to_string(&index).unwrap()).unwrap();

        let adapter = match try_load(dir.path()) {
            LoadResult::Loaded(a) => a,
            _ => panic!("expected Loaded"),
        };

        let handle = adapter
            .handle_for_member(&dir.path().join("src/lib.rs"), "create_user")
            .expect("mem handle for create_user");
        assert!(
            handle.starts_with("mem:"),
            "expected mem: handle, got {handle}"
        );
        // 4 + 16 = 20 hex chars after "mem:".
        assert_eq!(handle.len(), 4 + 16, "mem handle length: {handle}");
    }

    #[test]
    fn lookup_handle_round_trip() {
        let dir = tempdir().unwrap();
        write_minimal_project(dir.path());
        let opts = tp_index::BuildOptions {
            project_root: dir.path().to_path_buf(),
        };
        let index = tp_index::build(&opts).unwrap();
        let idx_path = tp_index::index_path(dir.path());
        fs::create_dir_all(idx_path.parent().unwrap()).unwrap();
        fs::write(&idx_path, serde_json::to_string(&index).unwrap()).unwrap();

        let adapter = match try_load(dir.path()) {
            LoadResult::Loaded(a) => a,
            _ => panic!("expected Loaded"),
        };

        let first_fam = adapter.index.families.first().expect("at least one family");
        let fam_handle = first_fam.handle.clone();
        match adapter.lookup_handle(&fam_handle) {
            Some(HandleResolution::Family(f)) => {
                assert_eq!(f.handle, fam_handle);
            }
            _ => panic!("expected Family resolution for {fam_handle}"),
        }

        let first_mem = first_fam.members.first().expect("at least one member");
        let mem_handle = first_mem.handle.clone();
        match adapter.lookup_handle(&mem_handle) {
            Some(HandleResolution::Member { member, family }) => {
                assert_eq!(member.handle, mem_handle);
                assert_eq!(family.handle, fam_handle);
            }
            _ => panic!("expected Member resolution for {mem_handle}"),
        }
    }

    fn debug_load(r: &LoadResult) -> String {
        match r {
            LoadResult::Loaded(_) => "Loaded".to_string(),
            LoadResult::Stale { reason, .. } => format!("Stale({reason})"),
            LoadResult::Missing => "Missing".to_string(),
        }
    }
}
