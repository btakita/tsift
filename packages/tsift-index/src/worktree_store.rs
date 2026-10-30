use anyhow::{bail, Context, Result};
use fs4::fs_std::FileExt;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::{Component, Path, PathBuf};
use tempfile::NamedTempFile;

/// Domain separator for immutable extraction artifacts shared by worktrees.
pub const ARTIFACT_KEY_SCHEMA: &str = "tsift-artifact-v1";

/// Result of publishing an immutable CAS object or manifest.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PublicationState {
    Published,
    AlreadyPresent,
}

/// On-disk owner for immutable extraction artifacts and persisted manifests.
///
/// Immutable objects use no-clobber publication. A concurrent publisher may
/// win, but it must have written identical bytes for the same key. Dirty
/// overlays are the only mutable records and are serialized by a checkout-
/// local sidecar lock before their atomically replaced JSON is published.
#[derive(Debug, Clone)]
pub struct WorktreeStore {
    root: PathBuf,
}

impl WorktreeStore {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn artifact_path(&self, artifact_key: &str) -> Result<PathBuf> {
        validate_digest("artifact key", artifact_key)?;
        Ok(self
            .root
            .join("cas/artifacts")
            .join(&artifact_key[..2])
            .join(format!("{artifact_key}.artifact")))
    }

    pub fn publish_artifact(&self, artifact_key: &str, payload: &[u8]) -> Result<PublicationState> {
        let path = self.artifact_path(artifact_key)?;
        publish_immutable(&path, payload)
    }

    pub fn read_artifact(&self, artifact_key: &str) -> Result<Option<Vec<u8>>> {
        read_optional(&self.artifact_path(artifact_key)?)
    }

    pub fn commit_manifest_path(&self, manifest: &CommitManifest) -> Result<PathBuf> {
        manifest.validate()?;
        self.commit_manifest_path_for(
            &manifest.repository_id,
            &manifest.commit_oid,
            &manifest.tree_oid,
        )
    }

    fn commit_manifest_path_for(
        &self,
        repository_id: &str,
        commit_oid: &str,
        tree_oid: &str,
    ) -> Result<PathBuf> {
        validate_nonempty_id("repository_id", repository_id)?;
        validate_git_oid("commit_oid", commit_oid)?;
        validate_git_oid("tree_oid", tree_oid)?;
        Ok(self
            .root
            .join("manifests/commits")
            .join(stable_name(repository_id))
            .join(format!("{commit_oid}-{tree_oid}.json")))
    }

    pub fn publish_commit_manifest(&self, manifest: &CommitManifest) -> Result<PublicationState> {
        let path = self.commit_manifest_path(manifest)?;
        let payload = canonical_json(manifest).context("serialize commit manifest")?;
        publish_immutable(&path, &payload)
    }

    pub fn read_commit_manifest(
        &self,
        repository_id: &str,
        commit_oid: &str,
        tree_oid: &str,
    ) -> Result<Option<CommitManifest>> {
        let path = self.commit_manifest_path_for(repository_id, commit_oid, tree_oid)?;
        let Some(payload) = read_optional(&path)? else {
            return Ok(None);
        };
        let manifest: CommitManifest =
            serde_json::from_slice(&payload).context("parse commit manifest")?;
        manifest.validate()?;
        if manifest.repository_id != repository_id
            || manifest.commit_oid != commit_oid
            || manifest.tree_oid != tree_oid
        {
            bail!("commit manifest identity does not match its storage address");
        }
        Ok(Some(manifest))
    }

    pub fn dirty_overlay_path(&self, checkout_id: &str) -> Result<PathBuf> {
        validate_nonempty_id("checkout_id", checkout_id)?;
        Ok(self
            .root
            .join("manifests/overlays")
            .join(format!("{}.json", stable_name(checkout_id))))
    }

    pub fn write_dirty_overlay(&self, overlay: &DirtyOverlayManifest) -> Result<()> {
        overlay.validate()?;
        let path = self.dirty_overlay_path(&overlay.checkout_id)?;
        let lock_path = path.with_extension("lock");
        ensure_parent(&lock_path)?;
        let lock = OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .truncate(false)
            .open(&lock_path)
            .with_context(|| format!("open overlay lock {}", lock_path.display()))?;
        lock.lock_exclusive()
            .with_context(|| format!("lock overlay {}", lock_path.display()))?;
        let payload = canonical_json(overlay).context("serialize dirty overlay")?;
        let result = atomic_replace(&path, &payload);
        let _ = FileExt::unlock(&lock);
        result
    }

    pub fn read_dirty_overlay(&self, checkout_id: &str) -> Result<Option<DirtyOverlayManifest>> {
        let path = self.dirty_overlay_path(checkout_id)?;
        let Some(payload) = read_optional(&path)? else {
            return Ok(None);
        };
        let overlay: DirtyOverlayManifest =
            serde_json::from_slice(&payload).context("parse dirty overlay")?;
        overlay.validate()?;
        if overlay.checkout_id != checkout_id {
            bail!(
                "dirty overlay identity mismatch: requested {checkout_id:?}, found {:?}",
                overlay.checkout_id
            );
        }
        Ok(Some(overlay))
    }
}

/// Inputs that make an extracted source artifact safe to reuse across checkouts.
///
/// A source blob alone is insufficient: configuration can change language
/// selection, and grammar/extractor/toolchain changes can change the emitted
/// symbol and edge records for identical bytes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ArtifactKeyInput {
    pub blob_oid: String,
    pub config_digest: String,
    pub toolchain_digest: String,
    pub extractor_version: String,
}

impl ArtifactKeyInput {
    pub fn validate(&self) -> Result<()> {
        validate_git_oid("blob_oid", &self.blob_oid)?;
        validate_digest("config_digest", &self.config_digest)?;
        validate_digest("toolchain_digest", &self.toolchain_digest)?;
        if self.extractor_version.trim().is_empty() {
            bail!("extractor_version must not be empty");
        }
        Ok(())
    }

    /// Stable BLAKE3 key for one immutable extraction artifact.
    pub fn digest_hex(&self) -> Result<String> {
        self.validate()?;
        let mut hasher = blake3::Hasher::new();
        hash_field(&mut hasher, ARTIFACT_KEY_SCHEMA);
        hash_field(&mut hasher, &self.blob_oid);
        hash_field(&mut hasher, &self.config_digest);
        hash_field(&mut hasher, &self.toolchain_digest);
        hash_field(&mut hasher, &self.extractor_version);
        Ok(hasher.finalize().to_hex().to_string())
    }
}

/// Immutable path-to-artifact projection for one Git commit/tree.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CommitManifest {
    pub repository_id: String,
    pub commit_oid: String,
    pub tree_oid: String,
    pub artifacts: BTreeMap<String, String>,
}

impl CommitManifest {
    pub fn validate(&self) -> Result<()> {
        if self.repository_id.trim().is_empty() {
            bail!("repository_id must not be empty");
        }
        validate_git_oid("commit_oid", &self.commit_oid)?;
        validate_git_oid("tree_oid", &self.tree_oid)?;
        validate_artifact_map(&self.artifacts)
    }
}

/// Worktree-local changes layered over an immutable commit manifest.
///
/// `writes` maps dirty paths to immutable artifact keys. `tombstones` removes
/// paths that still exist in the base tree. The same path cannot be both.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DirtyOverlayManifest {
    pub checkout_id: String,
    pub base_tree_oid: String,
    pub writes: BTreeMap<String, String>,
    pub tombstones: BTreeSet<String>,
}

impl DirtyOverlayManifest {
    pub fn validate(&self) -> Result<()> {
        if self.checkout_id.trim().is_empty() {
            bail!("checkout_id must not be empty");
        }
        validate_git_oid("base_tree_oid", &self.base_tree_oid)?;
        validate_artifact_map(&self.writes)?;
        for path in &self.tombstones {
            validate_manifest_path(path)?;
            if self.writes.contains_key(path) {
                bail!("overlay path {path:?} cannot be both a write and a tombstone");
            }
        }
        Ok(())
    }

    pub fn resolves_to<'a>(&'a self, base: &'a CommitManifest, path: &str) -> Option<&'a str> {
        if self.tombstones.contains(path) {
            None
        } else {
            self.writes
                .get(path)
                .or_else(|| base.artifacts.get(path))
                .map(String::as_str)
        }
    }
}

fn hash_field(hasher: &mut blake3::Hasher, value: &str) {
    hasher.update(&(value.len() as u64).to_le_bytes());
    hasher.update(value.as_bytes());
}

fn validate_git_oid(field: &str, value: &str) -> Result<()> {
    if !matches!(value.len(), 40 | 64) || !value.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        bail!("{field} must be a 40- or 64-character hexadecimal Git object id");
    }
    Ok(())
}

fn validate_digest(field: &str, value: &str) -> Result<()> {
    if value.len() != 64 || !value.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        bail!("{field} must be a 64-character hexadecimal digest");
    }
    Ok(())
}

fn validate_artifact_map(artifacts: &BTreeMap<String, String>) -> Result<()> {
    for (path, artifact_key) in artifacts {
        validate_manifest_path(path)?;
        validate_digest("artifact key", artifact_key)?;
    }
    Ok(())
}

fn validate_manifest_path(value: &str) -> Result<()> {
    let path = Path::new(value);
    if value.is_empty()
        || path.is_absolute()
        || path.components().any(|component| {
            matches!(
                component,
                Component::Prefix(_)
                    | Component::RootDir
                    | Component::ParentDir
                    | Component::CurDir
            )
        })
    {
        bail!("manifest path {value:?} must be a normalized repository-relative path");
    }
    Ok(())
}

fn validate_nonempty_id(field: &str, value: &str) -> Result<()> {
    if value.trim().is_empty() {
        bail!("{field} must not be empty");
    }
    Ok(())
}

fn stable_name(value: &str) -> String {
    let mut hasher = blake3::Hasher::new();
    hash_field(&mut hasher, "tsift-worktree-store-name-v1");
    hash_field(&mut hasher, value);
    hasher.finalize().to_hex().to_string()
}

fn canonical_json<T: Serialize>(value: &T) -> Result<Vec<u8>> {
    let mut payload = serde_json::to_vec_pretty(value)?;
    payload.push(b'\n');
    Ok(payload)
}

fn ensure_parent(path: &Path) -> Result<&Path> {
    let parent = path
        .parent()
        .with_context(|| format!("path has no parent: {}", path.display()))?;
    let mut missing = Vec::new();
    let mut cursor = parent;
    while !cursor
        .try_exists()
        .with_context(|| format!("inspect store directory {}", cursor.display()))?
    {
        missing.push(cursor.to_path_buf());
        let Some(ancestor) = cursor
            .parent()
            .filter(|ancestor| !ancestor.as_os_str().is_empty())
        else {
            break;
        };
        cursor = ancestor;
    }
    fs::create_dir_all(parent)
        .with_context(|| format!("create store directory {}", parent.display()))?;
    for created in missing.iter().rev() {
        if let Some(created_parent) = created.parent() {
            let created_parent = if created_parent.as_os_str().is_empty() {
                Path::new(".")
            } else {
                created_parent
            };
            sync_directory(created_parent)?;
        }
    }
    Ok(parent)
}

fn read_optional(path: &Path) -> Result<Option<Vec<u8>>> {
    match fs::read(path) {
        Ok(payload) => Ok(Some(payload)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error).with_context(|| format!("read {}", path.display())),
    }
}

fn publish_immutable(path: &Path, payload: &[u8]) -> Result<PublicationState> {
    if let Some(existing) = read_optional(path)? {
        return immutable_existing(path, payload, &existing);
    }

    let parent = ensure_parent(path)?;
    let mut temporary = NamedTempFile::new_in(parent)
        .with_context(|| format!("create publication temp file in {}", parent.display()))?;
    temporary
        .write_all(payload)
        .with_context(|| format!("write publication temp file for {}", path.display()))?;
    temporary
        .as_file()
        .sync_all()
        .with_context(|| format!("sync publication temp file for {}", path.display()))?;

    match temporary.persist_noclobber(path) {
        Ok(_) => {
            sync_directory(parent)?;
            Ok(PublicationState::Published)
        }
        Err(error) if error.error.kind() == std::io::ErrorKind::AlreadyExists => {
            let existing = fs::read(path)
                .with_context(|| format!("read concurrent publication {}", path.display()))?;
            immutable_existing(path, payload, &existing)
        }
        Err(error) => {
            Err(error.error).with_context(|| format!("publish immutable object {}", path.display()))
        }
    }
}

fn immutable_existing(path: &Path, intended: &[u8], existing: &[u8]) -> Result<PublicationState> {
    if intended == existing {
        Ok(PublicationState::AlreadyPresent)
    } else {
        bail!(
            "immutable publication collision at {}: existing bytes differ",
            path.display()
        )
    }
}

fn atomic_replace(path: &Path, payload: &[u8]) -> Result<()> {
    let parent = ensure_parent(path)?;
    let mut temporary = NamedTempFile::new_in(parent)
        .with_context(|| format!("create replacement temp file in {}", parent.display()))?;
    temporary
        .write_all(payload)
        .with_context(|| format!("write replacement temp file for {}", path.display()))?;
    temporary
        .as_file()
        .sync_all()
        .with_context(|| format!("sync replacement temp file for {}", path.display()))?;
    temporary
        .persist(path)
        .map_err(|error| error.error)
        .with_context(|| format!("replace {}", path.display()))?;
    sync_directory(parent)
}

#[cfg(unix)]
fn sync_directory(path: &Path) -> Result<()> {
    File::open(path)
        .with_context(|| format!("open directory for sync {}", path.display()))?
        .sync_all()
        .with_context(|| format!("sync directory {}", path.display()))
}

#[cfg(not(unix))]
fn sync_directory(_path: &Path) -> Result<()> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Barrier};

    const SHA1_A: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    const SHA1_B: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
    const DIGEST_C: &str = "cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc";
    const DIGEST_D: &str = "dddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddd";

    fn artifact_input() -> ArtifactKeyInput {
        ArtifactKeyInput {
            blob_oid: SHA1_A.into(),
            config_digest: DIGEST_C.into(),
            toolchain_digest: DIGEST_D.into(),
            extractor_version: "rust-v3".into(),
        }
    }

    fn commit_manifest() -> CommitManifest {
        CommitManifest {
            repository_id: "example".into(),
            commit_oid: SHA1_A.into(),
            tree_oid: SHA1_B.into(),
            artifacts: BTreeMap::from([("src/lib.rs".into(), DIGEST_C.into())]),
        }
    }

    fn dirty_overlay() -> DirtyOverlayManifest {
        DirtyOverlayManifest {
            checkout_id: "worktree-feature".into(),
            base_tree_oid: SHA1_B.into(),
            writes: BTreeMap::from([("src/lib.rs".into(), DIGEST_C.into())]),
            tombstones: BTreeSet::from(["src/old.rs".into()]),
        }
    }

    #[test]
    fn artifact_key_is_deterministic_and_covers_every_semantic_input() {
        let baseline = artifact_input().digest_hex().unwrap();
        assert_eq!(baseline, artifact_input().digest_hex().unwrap());

        let variants = [
            ArtifactKeyInput {
                blob_oid: SHA1_B.into(),
                ..artifact_input()
            },
            ArtifactKeyInput {
                config_digest: DIGEST_D.into(),
                ..artifact_input()
            },
            ArtifactKeyInput {
                toolchain_digest: DIGEST_C.into(),
                ..artifact_input()
            },
            ArtifactKeyInput {
                extractor_version: "rust-v4".into(),
                ..artifact_input()
            },
        ];
        for variant in variants {
            assert_ne!(baseline, variant.digest_hex().unwrap());
        }
    }

    #[test]
    fn commit_manifest_rejects_paths_outside_the_repository() {
        let mut artifacts = BTreeMap::new();
        artifacts.insert("../other-worktree/src/lib.rs".into(), DIGEST_C.into());
        let manifest = CommitManifest {
            repository_id: "example".into(),
            commit_oid: SHA1_A.into(),
            tree_oid: SHA1_B.into(),
            artifacts,
        };
        assert!(manifest.validate().is_err());
    }

    #[test]
    fn artifact_publication_is_idempotent_and_collision_safe() {
        let directory = tempfile::tempdir().unwrap();
        let store = WorktreeStore::new(directory.path().join("store"));

        assert_eq!(
            store.publish_artifact(DIGEST_C, b"artifact-v1").unwrap(),
            PublicationState::Published
        );
        assert_eq!(
            store.publish_artifact(DIGEST_C, b"artifact-v1").unwrap(),
            PublicationState::AlreadyPresent
        );
        assert!(store.publish_artifact(DIGEST_C, b"artifact-v2").is_err());
        assert_eq!(
            store.read_artifact(DIGEST_C).unwrap().unwrap(),
            b"artifact-v1"
        );
    }

    #[test]
    fn relative_store_root_publishes_with_durable_directory_creation() {
        let directory = tempfile::tempdir_in(".").unwrap();
        let current = std::env::current_dir().unwrap();
        let relative = directory
            .path()
            .strip_prefix(current)
            .unwrap()
            .join("store");
        assert!(!relative.is_absolute());
        let store = WorktreeStore::new(relative);

        assert_eq!(
            store.publish_artifact(DIGEST_C, b"artifact").unwrap(),
            PublicationState::Published
        );
        assert_eq!(store.read_artifact(DIGEST_C).unwrap().unwrap(), b"artifact");
    }

    #[test]
    fn concurrent_artifact_publishers_never_clobber_the_winner() {
        let directory = tempfile::tempdir().unwrap();
        let store = Arc::new(WorktreeStore::new(directory.path().join("store")));
        let barrier = Arc::new(Barrier::new(3));
        let mut publishers = Vec::new();

        for payload in [b"artifact-a".as_slice(), b"artifact-b".as_slice()] {
            let store = Arc::clone(&store);
            let barrier = Arc::clone(&barrier);
            publishers.push(std::thread::spawn(move || {
                barrier.wait();
                store.publish_artifact(DIGEST_C, payload)
            }));
        }
        barrier.wait();
        let results: Vec<_> = publishers
            .into_iter()
            .map(|publisher| publisher.join().unwrap())
            .collect();

        assert_eq!(results.iter().filter(|result| result.is_ok()).count(), 1);
        let stored = store.read_artifact(DIGEST_C).unwrap().unwrap();
        assert!(stored == b"artifact-a" || stored == b"artifact-b");
    }

    #[test]
    fn commit_manifest_round_trips_and_is_immutable() {
        let directory = tempfile::tempdir().unwrap();
        let store = WorktreeStore::new(directory.path().join("store"));
        let manifest = commit_manifest();
        let path = store.commit_manifest_path(&manifest).unwrap();

        assert_eq!(
            store.publish_commit_manifest(&manifest).unwrap(),
            PublicationState::Published
        );
        assert!(path.exists());
        assert_eq!(
            store
                .read_commit_manifest(
                    &manifest.repository_id,
                    &manifest.commit_oid,
                    &manifest.tree_oid,
                )
                .unwrap()
                .unwrap(),
            manifest
        );
        assert_eq!(
            store.publish_commit_manifest(&manifest).unwrap(),
            PublicationState::AlreadyPresent
        );

        let mut conflicting = manifest.clone();
        conflicting
            .artifacts
            .insert("src/main.rs".into(), DIGEST_D.into());
        assert!(store.publish_commit_manifest(&conflicting).is_err());
        assert_eq!(
            store
                .read_commit_manifest(
                    &manifest.repository_id,
                    &manifest.commit_oid,
                    &manifest.tree_oid,
                )
                .unwrap()
                .unwrap(),
            manifest
        );
    }

    #[test]
    fn dirty_overlay_replacement_is_atomic_and_ignores_abandoned_temps() {
        let directory = tempfile::tempdir().unwrap();
        let store = WorktreeStore::new(directory.path().join("store"));
        let mut overlay = dirty_overlay();
        store.write_dirty_overlay(&overlay).unwrap();
        assert_eq!(
            store
                .read_dirty_overlay(&overlay.checkout_id)
                .unwrap()
                .unwrap(),
            overlay
        );

        let path = store.dirty_overlay_path(&overlay.checkout_id).unwrap();
        fs::write(path.parent().unwrap().join(".abandoned-temp"), b"truncated").unwrap();
        overlay.writes.insert("src/main.rs".into(), DIGEST_D.into());
        store.write_dirty_overlay(&overlay).unwrap();

        assert_eq!(
            store
                .read_dirty_overlay(&overlay.checkout_id)
                .unwrap()
                .unwrap(),
            overlay
        );
        assert!(path.with_extension("lock").exists());
    }

    #[test]
    fn dirty_overlay_write_wins_and_tombstone_removes_base_path() {
        let mut base_artifacts = BTreeMap::new();
        base_artifacts.insert("src/lib.rs".into(), DIGEST_C.into());
        base_artifacts.insert("src/old.rs".into(), DIGEST_C.into());
        let base = CommitManifest {
            repository_id: "example".into(),
            commit_oid: SHA1_A.into(),
            tree_oid: SHA1_B.into(),
            artifacts: base_artifacts,
        };

        let mut writes = BTreeMap::new();
        writes.insert("src/lib.rs".into(), DIGEST_D.into());
        let overlay = DirtyOverlayManifest {
            checkout_id: "worktree-feature".into(),
            base_tree_oid: SHA1_B.into(),
            writes,
            tombstones: BTreeSet::from(["src/old.rs".into()]),
        };

        base.validate().unwrap();
        overlay.validate().unwrap();
        assert_eq!(overlay.resolves_to(&base, "src/lib.rs"), Some(DIGEST_D));
        assert_eq!(overlay.resolves_to(&base, "src/old.rs"), None);
    }

    #[test]
    fn dirty_overlay_rejects_write_tombstone_overlap() {
        let overlay = DirtyOverlayManifest {
            checkout_id: "worktree-feature".into(),
            base_tree_oid: SHA1_A.into(),
            writes: BTreeMap::from([("src/lib.rs".into(), DIGEST_C.into())]),
            tombstones: BTreeSet::from(["src/lib.rs".into()]),
        };
        assert!(overlay.validate().is_err());
    }
}
