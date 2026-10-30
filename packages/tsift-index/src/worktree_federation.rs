//! Explicit, provenance-preserving federation across worktree-local indexes.
//!
//! Federation is deliberately opt-in: callers must construct an
//! [`ExplicitPeerSelection`] and every [`FederatedHit`] contains validated
//! checkout provenance. Mutable overlay work is coordinated separately by
//! [`OverlayWriteScheduler`], which permits concurrent work on different
//! overlays but never yields two active writes for the same overlay.

use crate::worktree_store::{CommitManifest, DirtyOverlayManifest};
use anyhow::{Result, bail};
use serde::Serialize;
use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::path::{Component, Path};

/// Whether a checkout points at a branch or a detached `HEAD`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", content = "name", rename_all = "snake_case")]
pub enum HeadState {
    Branch(String),
    Detached,
}

impl HeadState {
    fn validate(&self) -> Result<()> {
        if let Self::Branch(name) = self
            && name.trim().is_empty()
        {
            bail!("branch name must not be empty");
        }
        Ok(())
    }
}

/// Stable identity attached to every result from a federated checkout.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct WorktreeProvenance {
    repository_id: String,
    checkout_id: String,
    worktree_root: String,
    head: HeadState,
    commit_oid: String,
    tree_oid: String,
}

impl WorktreeProvenance {
    pub fn new(
        repository_id: impl Into<String>,
        checkout_id: impl Into<String>,
        worktree_root: impl Into<String>,
        head: HeadState,
        commit_oid: impl Into<String>,
        tree_oid: impl Into<String>,
    ) -> Result<Self> {
        let provenance = Self {
            repository_id: repository_id.into(),
            checkout_id: checkout_id.into(),
            worktree_root: worktree_root.into(),
            head,
            commit_oid: commit_oid.into(),
            tree_oid: tree_oid.into(),
        };
        provenance.validate()?;
        Ok(provenance)
    }

    pub fn repository_id(&self) -> &str {
        &self.repository_id
    }

    pub fn checkout_id(&self) -> &str {
        &self.checkout_id
    }

    pub fn worktree_root(&self) -> &str {
        &self.worktree_root
    }

    pub fn head(&self) -> &HeadState {
        &self.head
    }

    pub fn commit_oid(&self) -> &str {
        &self.commit_oid
    }

    pub fn tree_oid(&self) -> &str {
        &self.tree_oid
    }

    fn validate(&self) -> Result<()> {
        validate_identifier("repository_id", &self.repository_id)?;
        validate_identifier("checkout_id", &self.checkout_id)?;
        validate_worktree_root(&self.worktree_root)?;
        self.head.validate()?;
        validate_git_oid("commit_oid", &self.commit_oid)?;
        validate_git_oid("tree_oid", &self.tree_oid)
    }
}

/// One selectable worktree, composed from validated immutable and overlay manifests.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FederationPeer {
    provenance: WorktreeProvenance,
    base: CommitManifest,
    overlay: DirtyOverlayManifest,
}

impl FederationPeer {
    pub fn new(
        provenance: WorktreeProvenance,
        base: CommitManifest,
        overlay: DirtyOverlayManifest,
    ) -> Result<Self> {
        provenance.validate()?;
        base.validate()?;
        overlay.validate()?;
        if provenance.repository_id != base.repository_id {
            bail!("peer provenance and base manifest repository ids differ");
        }
        if provenance.commit_oid != base.commit_oid || provenance.tree_oid != base.tree_oid {
            bail!("peer provenance and base manifest Git identities differ");
        }
        if provenance.checkout_id != overlay.checkout_id {
            bail!("peer provenance and overlay checkout ids differ");
        }
        if provenance.tree_oid != overlay.base_tree_oid {
            bail!("peer provenance and overlay base tree ids differ");
        }
        Ok(Self {
            provenance,
            base,
            overlay,
        })
    }

    pub fn provenance(&self) -> &WorktreeProvenance {
        &self.provenance
    }

    pub fn base(&self) -> &CommitManifest {
        &self.base
    }

    pub fn overlay(&self) -> &DirtyOverlayManifest {
        &self.overlay
    }

    pub fn resolves_to(&self, source_path: &str) -> Option<&str> {
        self.overlay.resolves_to(&self.base, source_path)
    }

    pub fn hit<T>(&self, source_path: impl Into<String>, value: T) -> Result<FederatedHit<T>> {
        FederatedHit::new(self.provenance.clone(), source_path, value)
    }
}

/// A non-empty, duplicate-free list of worktree peers selected by the caller.
///
/// There is intentionally no implicit or default selection constructor.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExplicitPeerSelection {
    checkout_ids: Vec<String>,
}

impl ExplicitPeerSelection {
    pub fn new(checkout_ids: impl IntoIterator<Item = String>) -> Result<Self> {
        let checkout_ids: Vec<_> = checkout_ids.into_iter().collect();
        if checkout_ids.is_empty() {
            bail!("federation requires at least one explicitly selected checkout");
        }

        let mut seen = BTreeSet::new();
        for checkout_id in &checkout_ids {
            validate_identifier("checkout_id", checkout_id)?;
            if !seen.insert(checkout_id) {
                bail!("checkout {checkout_id:?} was selected more than once");
            }
        }
        Ok(Self { checkout_ids })
    }

    pub fn checkout_ids(&self) -> &[String] {
        &self.checkout_ids
    }
}

/// Validated peer inventory for one repository.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FederationCatalog {
    repository_id: String,
    peers: BTreeMap<String, FederationPeer>,
}

impl FederationCatalog {
    pub fn new(
        repository_id: impl Into<String>,
        peers: impl IntoIterator<Item = FederationPeer>,
    ) -> Result<Self> {
        let repository_id = repository_id.into();
        validate_identifier("repository_id", &repository_id)?;

        let mut by_checkout = BTreeMap::new();
        for peer in peers {
            if peer.provenance.repository_id != repository_id {
                bail!(
                    "checkout {:?} belongs to repository {:?}, not {:?}",
                    peer.provenance.checkout_id,
                    peer.provenance.repository_id,
                    repository_id
                );
            }
            let checkout_id = peer.provenance.checkout_id.clone();
            if by_checkout.insert(checkout_id.clone(), peer).is_some() {
                bail!("duplicate federation peer {checkout_id:?}");
            }
        }

        Ok(Self {
            repository_id,
            peers: by_checkout,
        })
    }

    pub fn repository_id(&self) -> &str {
        &self.repository_id
    }

    /// Resolves only the peers named by `selection`, preserving caller order.
    pub fn select(&self, selection: &ExplicitPeerSelection) -> Result<Vec<&FederationPeer>> {
        selection
            .checkout_ids
            .iter()
            .map(|checkout_id| {
                self.peers
                    .get(checkout_id)
                    .ok_or_else(|| anyhow::anyhow!("unknown federation peer {checkout_id:?}"))
            })
            .collect()
    }
}

/// One federated result whose source checkout cannot be omitted.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct FederatedHit<T> {
    provenance: WorktreeProvenance,
    source_path: String,
    value: T,
}

impl<T> FederatedHit<T> {
    pub fn new(
        provenance: WorktreeProvenance,
        source_path: impl Into<String>,
        value: T,
    ) -> Result<Self> {
        provenance.validate()?;
        let source_path = source_path.into();
        validate_source_path(&source_path)?;
        Ok(Self {
            provenance,
            source_path,
            value,
        })
    }

    pub fn provenance(&self) -> &WorktreeProvenance {
        &self.provenance
    }

    pub fn source_path(&self) -> &str {
        &self.source_path
    }

    pub fn value(&self) -> &T {
        &self.value
    }

    pub fn into_parts(self) -> (WorktreeProvenance, String, T) {
        (self.provenance, self.source_path, self.value)
    }
}

/// A write yielded by [`OverlayWriteScheduler`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScheduledOverlayWrite<T> {
    pub overlay_id: String,
    pub value: T,
}

/// FIFO scheduler that enforces a single active writer per dirty overlay.
#[derive(Debug)]
pub struct OverlayWriteScheduler<T> {
    queues: BTreeMap<String, VecDeque<T>>,
    ready: VecDeque<String>,
    ready_set: BTreeSet<String>,
    active: BTreeSet<String>,
}

impl<T> Default for OverlayWriteScheduler<T> {
    fn default() -> Self {
        Self::new()
    }
}

impl<T> OverlayWriteScheduler<T> {
    pub fn new() -> Self {
        Self {
            queues: BTreeMap::new(),
            ready: VecDeque::new(),
            ready_set: BTreeSet::new(),
            active: BTreeSet::new(),
        }
    }

    pub fn enqueue(&mut self, overlay: &DirtyOverlayManifest, value: T) -> Result<()> {
        overlay.validate()?;
        let overlay_id = overlay.checkout_id.clone();
        let queue = self.queues.entry(overlay_id.clone()).or_default();
        let was_empty = queue.is_empty();
        queue.push_back(value);

        if was_empty && !self.active.contains(&overlay_id) {
            self.mark_ready(overlay_id);
        }
        Ok(())
    }

    /// Yields the oldest ready overlay write and marks that overlay active.
    ///
    /// Returns `None` when all queued work belongs to active overlays.
    pub fn take_ready(&mut self) -> Option<ScheduledOverlayWrite<T>> {
        while let Some(overlay_id) = self.ready.pop_front() {
            self.ready_set.remove(&overlay_id);
            if self.active.contains(&overlay_id) {
                continue;
            }

            let (value, empty) = {
                let queue = self.queues.get_mut(&overlay_id)?;
                (queue.pop_front()?, queue.is_empty())
            };
            if empty {
                self.queues.remove(&overlay_id);
            }
            self.active.insert(overlay_id.clone());
            return Some(ScheduledOverlayWrite { overlay_id, value });
        }
        None
    }

    /// Releases an overlay after its active write has committed or aborted.
    pub fn complete(&mut self, overlay_id: &str) -> Result<()> {
        if !self.active.remove(overlay_id) {
            bail!("overlay {overlay_id:?} has no active writer");
        }
        if self.queues.contains_key(overlay_id) {
            self.mark_ready(overlay_id.to_owned());
        }
        Ok(())
    }

    pub fn is_active(&self, overlay_id: &str) -> bool {
        self.active.contains(overlay_id)
    }

    pub fn queued_for(&self, overlay_id: &str) -> usize {
        self.queues.get(overlay_id).map_or(0, VecDeque::len)
    }

    fn mark_ready(&mut self, overlay_id: String) {
        if self.ready_set.insert(overlay_id.clone()) {
            self.ready.push_back(overlay_id);
        }
    }
}

fn validate_identifier(field: &str, value: &str) -> Result<()> {
    if value.trim().is_empty() {
        bail!("{field} must not be empty");
    }
    Ok(())
}

fn validate_git_oid(field: &str, value: &str) -> Result<()> {
    if !matches!(value.len(), 40 | 64) || !value.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        bail!("{field} must be a 40- or 64-character hexadecimal Git object id");
    }
    Ok(())
}

fn validate_worktree_root(value: &str) -> Result<()> {
    let path = Path::new(value);
    if !path.is_absolute()
        || path
            .components()
            .any(|component| matches!(component, Component::ParentDir | Component::CurDir))
    {
        bail!("worktree_root must be a normalized absolute path");
    }
    Ok(())
}

fn validate_source_path(value: &str) -> Result<()> {
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
        bail!("source_path {value:?} must be a normalized repository-relative path");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const COMMIT: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    const TREE: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
    const ARTIFACT_BASE: &str = "cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc";
    const ARTIFACT_DIRTY: &str = "dddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddd";

    fn provenance(checkout_id: &str) -> WorktreeProvenance {
        WorktreeProvenance::new(
            "example",
            checkout_id,
            format!("/repo/worktrees/{checkout_id}"),
            HeadState::Branch(format!("feature/{checkout_id}")),
            COMMIT,
            TREE,
        )
        .unwrap()
    }

    fn base() -> CommitManifest {
        CommitManifest {
            repository_id: "example".into(),
            commit_oid: COMMIT.into(),
            tree_oid: TREE.into(),
            artifacts: BTreeMap::from([("src/lib.rs".into(), ARTIFACT_BASE.into())]),
        }
    }

    fn overlay(checkout_id: &str) -> DirtyOverlayManifest {
        DirtyOverlayManifest {
            checkout_id: checkout_id.into(),
            base_tree_oid: TREE.into(),
            writes: BTreeMap::from([("src/lib.rs".into(), ARTIFACT_DIRTY.into())]),
            tombstones: BTreeSet::new(),
        }
    }

    fn peer(checkout_id: &str) -> FederationPeer {
        FederationPeer::new(provenance(checkout_id), base(), overlay(checkout_id)).unwrap()
    }

    #[test]
    fn federation_requires_explicit_known_unique_peers() {
        let catalog = FederationCatalog::new("example", [peer("alpha"), peer("beta")]).unwrap();
        let selection = ExplicitPeerSelection::new(vec!["beta".into(), "alpha".into()]).unwrap();
        let selected = catalog.select(&selection).unwrap();
        assert_eq!(selected[0].provenance().checkout_id(), "beta");
        assert_eq!(selected[1].provenance().checkout_id(), "alpha");
        assert_eq!(selected[0].resolves_to("src/lib.rs"), Some(ARTIFACT_DIRTY));

        assert!(ExplicitPeerSelection::new(Vec::new()).is_err());
        assert!(ExplicitPeerSelection::new(vec!["alpha".into(), "alpha".into()]).is_err());
        let unknown = ExplicitPeerSelection::new(vec!["missing".into()]).unwrap();
        assert!(catalog.select(&unknown).is_err());
    }

    #[test]
    fn federated_hits_serialize_all_mandatory_provenance() {
        let hit = peer("alpha").hit("src/lib.rs", "match").unwrap();
        let json = serde_json::to_value(&hit).unwrap();
        assert_eq!(json["provenance"]["repository_id"], "example");
        assert_eq!(json["provenance"]["checkout_id"], "alpha");
        assert_eq!(json["provenance"]["worktree_root"], "/repo/worktrees/alpha");
        assert_eq!(json["provenance"]["head"]["kind"], "branch");
        assert_eq!(json["provenance"]["commit_oid"], COMMIT);
        assert_eq!(json["provenance"]["tree_oid"], TREE);
        assert_eq!(json["source_path"], "src/lib.rs");
        assert!(peer("alpha").hit("../sibling/src/lib.rs", "match").is_err());
    }

    #[test]
    fn scheduler_serializes_each_overlay_without_globally_serializing_writes() {
        let mut scheduler = OverlayWriteScheduler::new();
        let alpha = overlay("alpha");
        let beta = overlay("beta");
        scheduler.enqueue(&alpha, "alpha-1").unwrap();
        scheduler.enqueue(&alpha, "alpha-2").unwrap();
        scheduler.enqueue(&beta, "beta-1").unwrap();

        assert_eq!(
            scheduler.take_ready(),
            Some(ScheduledOverlayWrite {
                overlay_id: "alpha".into(),
                value: "alpha-1"
            })
        );
        assert_eq!(
            scheduler.take_ready(),
            Some(ScheduledOverlayWrite {
                overlay_id: "beta".into(),
                value: "beta-1"
            })
        );
        assert!(scheduler.take_ready().is_none());
        assert_eq!(scheduler.queued_for("alpha"), 1);

        scheduler.complete("alpha").unwrap();
        assert_eq!(
            scheduler.take_ready(),
            Some(ScheduledOverlayWrite {
                overlay_id: "alpha".into(),
                value: "alpha-2"
            })
        );
        assert!(scheduler.complete("missing").is_err());
    }
}
