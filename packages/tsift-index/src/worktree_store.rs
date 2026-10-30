use anyhow::{Result, bail};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Component, Path};

/// Domain separator for immutable extraction artifacts shared by worktrees.
pub const ARTIFACT_KEY_SCHEMA: &str = "tsift-artifact-v1";

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

#[cfg(test)]
mod tests {
    use super::*;

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
