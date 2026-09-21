//! Ambient-state roots: paths that hold tsift's own user-level state (or a
//! shared scratch area) rather than a workspace.
//!
//! tsift writes user-level state into `~/.tsift/` — the GPU lease, prompt-cache
//! history, artifacts — which *creates that directory*. Root resolution walks
//! ancestors looking for a `.tsift/` directory, so once any tsift run has
//! touched user-level state, `$HOME` starts looking like a workspace root. A
//! raw read of any file under `$HOME` but outside a repository then roots at
//! `$HOME` and tries to index the entire home directory. `$TMPDIR` and the
//! filesystem root are ambient for the same reason.
//!
//! Two independent defenses live here:
//!
//! * [`ambient_state_roots`] tells root *resolution* never to pick one of these
//!   on the strength of a bare `.tsift/` marker (an explicit `.git` repo at one
//!   of them is user-created and still counts).
//! * [`ensure_indexable_root`] is the backstop at the index *build* boundary. It
//!   refuses regardless of how the root was chosen — an operator typo, a hook
//!   that computed the root wrongly, `cd ~ && tsift index`, or a future
//!   regression in resolution itself — so the catastrophic case cannot be
//!   reached by a path that bypasses resolution.

use anyhow::{Result, bail};
use std::path::{Path, PathBuf};

/// Env escape hatch for [`ensure_indexable_root`].
pub const ALLOW_AMBIENT_ROOT_ENV: &str = "TSIFT_ALLOW_AMBIENT_ROOT";

/// Paths that are ambient state, never a workspace on the strength of a bare
/// `.tsift/` directory: `$TMPDIR`, `$HOME`, and the filesystem root.
pub fn ambient_state_roots() -> Vec<PathBuf> {
    let mut roots = Vec::new();
    if let Ok(temp_root) = std::env::temp_dir().canonicalize() {
        roots.push(temp_root);
    }
    if let Some(home) = home_dir_canonical() {
        roots.push(home);
    }
    roots.push(PathBuf::from("/"));
    roots
}

/// `$HOME`, canonicalized when possible.
pub fn home_dir_canonical() -> Option<PathBuf> {
    let home = std::env::var_os("HOME").filter(|value| !value.is_empty())?;
    let home = PathBuf::from(home);
    home.canonicalize().ok().or(Some(home))
}

/// Whether `root` is one of [`ambient_state_roots`].
pub fn is_ambient_state_root(root: &Path) -> bool {
    let canonical = root.canonicalize();
    let candidate = canonical.as_deref().unwrap_or(root);
    ambient_state_roots()
        .iter()
        .any(|ambient| ambient == candidate)
}

/// Pure parser for a truthy [`ALLOW_AMBIENT_ROOT_ENV`] value (factored out so
/// the rules can be unit-tested without mutating process env).
pub fn ambient_root_allow_value(value: &str) -> bool {
    matches!(
        value.trim().to_ascii_lowercase().as_str(),
        "1" | "true" | "yes" | "on"
    )
}

fn ambient_root_allowed() -> bool {
    std::env::var(ALLOW_AMBIENT_ROOT_ENV)
        .map(|value| ambient_root_allow_value(&value))
        .unwrap_or(false)
}

/// Refuse to build an index rooted at an ambient state root.
///
/// Indexing `$HOME` walks every cache, checkout, and build directory the user
/// owns. It has no legitimate use and one observed cost: an 11-hour, 97%-CPU
/// run with an 8GB WAL that committed nothing. Refuse by default and name the
/// escape hatch rather than making it unreachable.
pub fn ensure_indexable_root(root: &Path) -> Result<()> {
    ensure_indexable_root_with(root, ambient_root_allowed())
}

/// [`ensure_indexable_root`] with the escape hatch supplied explicitly.
pub fn ensure_indexable_root_with(root: &Path, allow_ambient: bool) -> Result<()> {
    if allow_ambient || !is_ambient_state_root(root) {
        return Ok(());
    }
    bail!(
        "refusing to index ambient state root {}: this is tsift's own user-state \
         directory (or a shared scratch area), not a workspace, and indexing it \
         walks every cache, checkout, and build directory beneath it. Point tsift \
         at the project root instead, or set {ALLOW_AMBIENT_ROOT_ENV}=1 to override.",
        root.display()
    )
}

/// Refuse to *create* an index whose owning root is an ambient state root.
///
/// Index databases live at `<root>/.tsift/...`, so the root is the parent of
/// the nearest `.tsift` ancestor. Checking here means a refused run never
/// creates the `~/.tsift/index.db` and `index.lock` it was about to reject —
/// `ensure_indexable_root` alone fires after `IndexDb::open` has already made
/// them. Both checks stay: this one keeps the filesystem clean, that one is the
/// backstop for roots that never pass through a db path.
pub fn ensure_indexable_db_path(db_path: &Path) -> Result<()> {
    let Some(root) = root_for_index_db_path(db_path) else {
        return Ok(());
    };
    ensure_indexable_root(&root)
}

/// The workspace root owning an index db path: the parent of its nearest
/// `.tsift` ancestor. `None` when the path is not under a `.tsift` directory.
pub fn root_for_index_db_path(db_path: &Path) -> Option<PathBuf> {
    db_path
        .ancestors()
        .find(|ancestor| ancestor.file_name().is_some_and(|name| name == ".tsift"))
        .and_then(Path::parent)
        .map(Path::to_path_buf)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ambient_state_roots_include_home_and_filesystem_root() {
        let roots = ambient_state_roots();
        assert!(
            roots.contains(&PathBuf::from("/")),
            "filesystem root missing from ambient roots: {roots:?}"
        );
        if let Some(home) = home_dir_canonical() {
            assert!(
                roots.contains(&home),
                "home dir {home:?} missing from ambient roots: {roots:?}"
            );
        }
    }

    #[test]
    fn filesystem_root_is_refused_as_an_index_root() {
        let err = ensure_indexable_root_with(Path::new("/"), false).unwrap_err();
        let message = err.to_string();
        assert!(
            message.contains("refusing to index ambient state root"),
            "unexpected message: {message}"
        );
        assert!(
            message.contains(ALLOW_AMBIENT_ROOT_ENV),
            "refusal must name its escape hatch: {message}"
        );
    }

    #[test]
    fn home_is_refused_as_an_index_root() {
        let Some(home) = home_dir_canonical() else {
            return;
        };
        assert!(ensure_indexable_root_with(&home, false).is_err());
    }

    #[test]
    fn escape_hatch_allows_an_ambient_root() {
        assert!(ensure_indexable_root_with(Path::new("/"), true).is_ok());
    }

    #[test]
    fn ordinary_project_root_is_indexable() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("project");
        std::fs::create_dir_all(&root).unwrap();
        ensure_indexable_root_with(&root, false).unwrap();
    }

    /// A tempdir lives *under* `$TMPDIR`; only `$TMPDIR` itself is ambient.
    #[test]
    fn a_directory_under_an_ambient_root_is_still_indexable() {
        let dir = tempfile::tempdir().unwrap();
        ensure_indexable_root_with(dir.path(), false).unwrap();
    }

    #[test]
    fn root_for_index_db_path_finds_the_tsift_parent() {
        assert_eq!(
            root_for_index_db_path(Path::new("/home/x/proj/.tsift/index.db")),
            Some(PathBuf::from("/home/x/proj"))
        );
        // Scoped multiplicity layout nests deeper under the same `.tsift`.
        assert_eq!(
            root_for_index_db_path(Path::new("/home/x/proj/.tsift/indexes/a/index.db")),
            Some(PathBuf::from("/home/x/proj"))
        );
        assert_eq!(root_for_index_db_path(Path::new("/home/x/proj/index.db")), None);
    }

    #[test]
    fn opening_an_index_db_under_an_ambient_root_is_refused() {
        let Some(home) = home_dir_canonical() else {
            return;
        };
        let err = ensure_indexable_db_path(&home.join(".tsift/index.db")).unwrap_err();
        assert!(
            err.to_string()
                .contains("refusing to index ambient state root")
        );
    }

    #[test]
    fn opening_an_index_db_under_a_real_project_is_allowed() {
        let dir = tempfile::tempdir().unwrap();
        ensure_indexable_db_path(&dir.path().join(".tsift/index.db")).unwrap();
    }

    #[test]
    fn allow_value_parses_the_documented_truthy_set() {
        for truthy in ["1", "true", "TRUE", " yes ", "On"] {
            assert!(ambient_root_allow_value(truthy), "{truthy:?} should allow");
        }
        for falsy in ["0", "false", "no", "off", "", "maybe"] {
            assert!(!ambient_root_allow_value(falsy), "{falsy:?} should refuse");
        }
    }
}
