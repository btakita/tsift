use anyhow::{Result, bail};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Component, Path};

/// One immutable artifact observed in the content-addressed store.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CasArtifact {
    pub artifact_key: String,
    pub published_at_unix_secs: u64,
    pub bytes: u64,
}

/// A live reference that prevents an artifact from being swept.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CasRoot {
    pub artifact_key: String,
    pub kind: CasRootKind,
    pub owner: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CasRootKind {
    CommitManifest,
    DirtyOverlay,
    Pin,
    InFlightPublication,
}

/// Root identity retained in plans and repair reports for observability.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct CasRootReference {
    pub kind: CasRootKind,
    pub owner: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct CasGcPolicy {
    pub grace_period_secs: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(tag = "reason", rename_all = "snake_case")]
pub enum CasRetentionReason {
    Root { root: CasRootReference },
    GracePeriod,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RetainedArtifact {
    pub artifact_key: String,
    pub published_at_unix_secs: u64,
    pub bytes: u64,
    pub reasons: BTreeSet<CasRetentionReason>,
}

/// An unreferenced artifact whose grace period had elapsed when the plan was made.
///
/// A sweeper must compare the observed publication timestamp and re-read all root
/// classes immediately before deletion. A plan is evidence, not a deletion lease.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CasSweepCandidate {
    pub artifact_key: String,
    pub observed_published_at_unix_secs: u64,
    pub bytes: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CasRepairAction {
    RebuildFromGit,
    RecoverDirtyOverlayFromCheckout,
    RestorePinnedArtifact,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CasRepairFinding {
    pub artifact_key: String,
    pub roots: BTreeSet<CasRootReference>,
    pub actions: BTreeSet<CasRepairAction>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CasGcMetrics {
    pub inventory_artifacts: u64,
    pub inventory_bytes: u64,
    pub rooted_artifacts: u64,
    pub rooted_bytes: u64,
    pub grace_held_artifacts: u64,
    pub grace_held_bytes: u64,
    pub reclaimable_artifacts: u64,
    pub reclaimable_bytes: u64,
    pub missing_artifacts: u64,
    pub missing_root_references: u64,
}

/// Deterministic, side-effect-free result of one GC inventory pass.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CasGcPlan {
    pub observed_at_unix_secs: u64,
    pub retained: Vec<RetainedArtifact>,
    pub sweep_candidates: Vec<CasSweepCandidate>,
    pub repair_findings: Vec<CasRepairFinding>,
    pub metrics: CasGcMetrics,
}

/// Plans grace-period collection from a consistent inventory and root snapshot.
///
/// Missing manifest, overlay, and pin references become repair findings. An
/// absent in-flight reference is not considered missing because publication may
/// not have reached the final inventory yet.
pub fn plan_cas_gc(
    observed_at_unix_secs: u64,
    policy: CasGcPolicy,
    inventory: &[CasArtifact],
    roots: &[CasRoot],
) -> Result<CasGcPlan> {
    let mut artifacts = BTreeMap::new();
    let mut metrics = CasGcMetrics::default();
    for artifact in inventory {
        validate_digest("artifact key", &artifact.artifact_key)?;
        if artifact.published_at_unix_secs > observed_at_unix_secs {
            bail!(
                "artifact {} has future publication timestamp {} (observation time {})",
                artifact.artifact_key,
                artifact.published_at_unix_secs,
                observed_at_unix_secs
            );
        }
        if artifacts
            .insert(artifact.artifact_key.clone(), artifact)
            .is_some()
        {
            bail!("duplicate inventory artifact {}", artifact.artifact_key);
        }
        checked_add(&mut metrics.inventory_artifacts, 1, "inventory count")?;
        checked_add(
            &mut metrics.inventory_bytes,
            artifact.bytes,
            "inventory bytes",
        )?;
    }

    let mut roots_by_artifact: BTreeMap<String, BTreeSet<CasRootReference>> = BTreeMap::new();
    for root in roots {
        validate_digest("root artifact key", &root.artifact_key)?;
        if root.owner.trim().is_empty() {
            bail!("CAS root owner must not be empty");
        }
        roots_by_artifact
            .entry(root.artifact_key.clone())
            .or_default()
            .insert(CasRootReference {
                kind: root.kind,
                owner: root.owner.clone(),
            });
    }

    let mut retained = Vec::new();
    let mut sweep_candidates = Vec::new();
    for artifact in artifacts.values() {
        let artifact_roots = roots_by_artifact.get(&artifact.artifact_key);
        if let Some(artifact_roots) = artifact_roots {
            let reasons = artifact_roots
                .iter()
                .cloned()
                .map(|root| CasRetentionReason::Root { root })
                .collect();
            retained.push(RetainedArtifact {
                artifact_key: artifact.artifact_key.clone(),
                published_at_unix_secs: artifact.published_at_unix_secs,
                bytes: artifact.bytes,
                reasons,
            });
            checked_add(&mut metrics.rooted_artifacts, 1, "rooted count")?;
            checked_add(&mut metrics.rooted_bytes, artifact.bytes, "rooted bytes")?;
            continue;
        }

        let age = observed_at_unix_secs - artifact.published_at_unix_secs;
        if age < policy.grace_period_secs {
            retained.push(RetainedArtifact {
                artifact_key: artifact.artifact_key.clone(),
                published_at_unix_secs: artifact.published_at_unix_secs,
                bytes: artifact.bytes,
                reasons: BTreeSet::from([CasRetentionReason::GracePeriod]),
            });
            checked_add(&mut metrics.grace_held_artifacts, 1, "grace-held count")?;
            checked_add(
                &mut metrics.grace_held_bytes,
                artifact.bytes,
                "grace-held bytes",
            )?;
        } else {
            sweep_candidates.push(CasSweepCandidate {
                artifact_key: artifact.artifact_key.clone(),
                observed_published_at_unix_secs: artifact.published_at_unix_secs,
                bytes: artifact.bytes,
            });
            checked_add(&mut metrics.reclaimable_artifacts, 1, "reclaimable count")?;
            checked_add(
                &mut metrics.reclaimable_bytes,
                artifact.bytes,
                "reclaimable bytes",
            )?;
        }
    }

    let mut repair_findings = Vec::new();
    for (artifact_key, artifact_roots) in roots_by_artifact {
        if artifacts.contains_key(&artifact_key) {
            continue;
        }
        let repair_roots: BTreeSet<_> = artifact_roots
            .into_iter()
            .filter(|root| root.kind != CasRootKind::InFlightPublication)
            .collect();
        if repair_roots.is_empty() {
            continue;
        }
        let actions = repair_roots.iter().map(repair_action).collect();
        checked_add(&mut metrics.missing_artifacts, 1, "missing artifact count")?;
        checked_add(
            &mut metrics.missing_root_references,
            repair_roots.len() as u64,
            "missing root reference count",
        )?;
        repair_findings.push(CasRepairFinding {
            artifact_key,
            roots: repair_roots,
            actions,
        });
    }

    Ok(CasGcPlan {
        observed_at_unix_secs,
        retained,
        sweep_candidates,
        repair_findings,
        metrics,
    })
}

fn repair_action(root: &CasRootReference) -> CasRepairAction {
    match root.kind {
        CasRootKind::CommitManifest => CasRepairAction::RebuildFromGit,
        CasRootKind::DirtyOverlay => CasRepairAction::RecoverDirtyOverlayFromCheckout,
        CasRootKind::Pin => CasRepairAction::RestorePinnedArtifact,
        CasRootKind::InFlightPublication => unreachable!("in-flight roots are not repair findings"),
    }
}

/// Persisted evidence available when opening a checkout's dirty overlay.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DirtyOverlayRecoveryState {
    pub checkout_id: String,
    pub primary_overlay_digest: Option<String>,
    pub recovery_overlay_digest: Option<String>,
    pub dirty_paths: BTreeSet<String>,
}

/// Safe action selected without modifying the checkout or its recovery copy.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "action", rename_all = "snake_case")]
pub enum DirtyOverlayRecoveryAction {
    UsePrimary { overlay_digest: String },
    RestoreRecovery { overlay_digest: String },
    RebuildCleanOverlay,
    PreserveDirtyCheckout { dirty_paths: BTreeSet<String> },
}

/// Chooses a fail-closed recovery action for a missing or damaged dirty overlay.
///
/// A checkout with dirty paths and no persisted overlay copy is never rebuilt
/// from Git because that would silently discard the only evidence of local work.
pub fn plan_dirty_overlay_recovery(
    state: &DirtyOverlayRecoveryState,
) -> Result<DirtyOverlayRecoveryAction> {
    if state.checkout_id.trim().is_empty() {
        bail!("checkout_id must not be empty");
    }
    if let Some(digest) = &state.primary_overlay_digest {
        validate_digest("primary overlay digest", digest)?;
    }
    if let Some(digest) = &state.recovery_overlay_digest {
        validate_digest("recovery overlay digest", digest)?;
    }
    for path in &state.dirty_paths {
        validate_relative_path(path)?;
    }

    if let Some(overlay_digest) = &state.primary_overlay_digest {
        Ok(DirtyOverlayRecoveryAction::UsePrimary {
            overlay_digest: overlay_digest.clone(),
        })
    } else if let Some(overlay_digest) = &state.recovery_overlay_digest {
        Ok(DirtyOverlayRecoveryAction::RestoreRecovery {
            overlay_digest: overlay_digest.clone(),
        })
    } else if state.dirty_paths.is_empty() {
        Ok(DirtyOverlayRecoveryAction::RebuildCleanOverlay)
    } else {
        Ok(DirtyOverlayRecoveryAction::PreserveDirtyCheckout {
            dirty_paths: state.dirty_paths.clone(),
        })
    }
}

fn validate_digest(field: &str, value: &str) -> Result<()> {
    if value.len() != 64 || !value.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        bail!("{field} must be a 64-character hexadecimal digest");
    }
    Ok(())
}

fn validate_relative_path(value: &str) -> Result<()> {
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
        bail!("dirty path {value:?} must be a normalized repository-relative path");
    }
    Ok(())
}

fn checked_add(target: &mut u64, value: u64, field: &str) -> Result<()> {
    *target = target
        .checked_add(value)
        .ok_or_else(|| anyhow::anyhow!("{field} overflow"))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const MANIFEST: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    const OVERLAY: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
    const PIN: &str = "cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc";
    const IN_FLIGHT: &str = "dddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddd";
    const YOUNG: &str = "eeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee";
    const OLD: &str = "ffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff";

    fn artifact(artifact_key: &str, published_at_unix_secs: u64, bytes: u64) -> CasArtifact {
        CasArtifact {
            artifact_key: artifact_key.into(),
            published_at_unix_secs,
            bytes,
        }
    }

    fn root(artifact_key: &str, kind: CasRootKind, owner: &str) -> CasRoot {
        CasRoot {
            artifact_key: artifact_key.into(),
            kind,
            owner: owner.into(),
        }
    }

    #[test]
    fn all_root_classes_protect_artifacts_and_grace_holds_young_objects() {
        let inventory = [
            artifact(MANIFEST, 10, 11),
            artifact(OVERLAY, 10, 12),
            artifact(PIN, 10, 13),
            artifact(IN_FLIGHT, 10, 14),
            artifact(YOUNG, 95, 15),
            artifact(OLD, 10, 16),
        ];
        let roots = [
            root(MANIFEST, CasRootKind::CommitManifest, "tree:abc"),
            root(OVERLAY, CasRootKind::DirtyOverlay, "checkout:feature"),
            root(PIN, CasRootKind::Pin, "snapshot:release"),
            root(IN_FLIGHT, CasRootKind::InFlightPublication, "publisher:123"),
        ];

        let plan = plan_cas_gc(
            100,
            CasGcPolicy {
                grace_period_secs: 20,
            },
            &inventory,
            &roots,
        )
        .unwrap();

        assert_eq!(plan.retained.len(), 5);
        assert_eq!(
            plan.sweep_candidates,
            vec![CasSweepCandidate {
                artifact_key: OLD.into(),
                observed_published_at_unix_secs: 10,
                bytes: 16,
            }]
        );
        assert_eq!(
            plan.metrics,
            CasGcMetrics {
                inventory_artifacts: 6,
                inventory_bytes: 81,
                rooted_artifacts: 4,
                rooted_bytes: 50,
                grace_held_artifacts: 1,
                grace_held_bytes: 15,
                reclaimable_artifacts: 1,
                reclaimable_bytes: 16,
                missing_artifacts: 0,
                missing_root_references: 0,
            }
        );
    }

    #[test]
    fn missing_durable_roots_have_actionable_repair_findings() {
        let roots = [
            root(MANIFEST, CasRootKind::CommitManifest, "tree:abc"),
            root(MANIFEST, CasRootKind::DirtyOverlay, "checkout:feature"),
            root(PIN, CasRootKind::Pin, "snapshot:release"),
            root(IN_FLIGHT, CasRootKind::InFlightPublication, "publisher:123"),
        ];

        let plan = plan_cas_gc(
            100,
            CasGcPolicy {
                grace_period_secs: 20,
            },
            &[],
            &roots,
        )
        .unwrap();

        assert_eq!(plan.repair_findings.len(), 2);
        assert_eq!(plan.repair_findings[0].artifact_key, MANIFEST);
        assert_eq!(
            plan.repair_findings[0].actions,
            BTreeSet::from([
                CasRepairAction::RebuildFromGit,
                CasRepairAction::RecoverDirtyOverlayFromCheckout,
            ])
        );
        assert_eq!(
            plan.repair_findings[1].actions,
            BTreeSet::from([CasRepairAction::RestorePinnedArtifact])
        );
        assert_eq!(plan.metrics.missing_artifacts, 2);
        assert_eq!(plan.metrics.missing_root_references, 3);
    }

    #[test]
    fn duplicate_root_reasons_are_deduplicated_but_distinct_owners_remain() {
        let repeated = root(MANIFEST, CasRootKind::CommitManifest, "tree:abc");
        let roots = [
            repeated.clone(),
            repeated,
            root(MANIFEST, CasRootKind::CommitManifest, "tree:def"),
        ];
        let plan = plan_cas_gc(
            100,
            CasGcPolicy {
                grace_period_secs: 20,
            },
            &[artifact(MANIFEST, 1, 1)],
            &roots,
        )
        .unwrap();

        assert_eq!(plan.retained[0].reasons.len(), 2);
    }

    #[test]
    fn dirty_overlay_recovery_prefers_persisted_state_and_never_discards_dirty_paths() {
        let primary = DirtyOverlayRecoveryState {
            checkout_id: "feature".into(),
            primary_overlay_digest: Some(MANIFEST.into()),
            recovery_overlay_digest: Some(OVERLAY.into()),
            dirty_paths: BTreeSet::from(["src/lib.rs".into()]),
        };
        assert_eq!(
            plan_dirty_overlay_recovery(&primary).unwrap(),
            DirtyOverlayRecoveryAction::UsePrimary {
                overlay_digest: MANIFEST.into()
            }
        );

        let recovery = DirtyOverlayRecoveryState {
            primary_overlay_digest: None,
            ..primary.clone()
        };
        assert!(matches!(
            plan_dirty_overlay_recovery(&recovery).unwrap(),
            DirtyOverlayRecoveryAction::RestoreRecovery { .. }
        ));

        let clean = DirtyOverlayRecoveryState {
            recovery_overlay_digest: None,
            dirty_paths: BTreeSet::new(),
            ..recovery.clone()
        };
        assert_eq!(
            plan_dirty_overlay_recovery(&clean).unwrap(),
            DirtyOverlayRecoveryAction::RebuildCleanOverlay
        );

        let dirty = DirtyOverlayRecoveryState {
            dirty_paths: BTreeSet::from(["src/lib.rs".into()]),
            ..clean
        };
        assert!(matches!(
            plan_dirty_overlay_recovery(&dirty).unwrap(),
            DirtyOverlayRecoveryAction::PreserveDirtyCheckout { .. }
        ));
    }

    #[test]
    fn invalid_or_inconsistent_inventory_fails_closed() {
        assert!(
            plan_cas_gc(
                100,
                CasGcPolicy {
                    grace_period_secs: 20
                },
                &[artifact(MANIFEST, 101, 1)],
                &[]
            )
            .is_err()
        );
        assert!(
            plan_cas_gc(
                100,
                CasGcPolicy {
                    grace_period_secs: 20
                },
                &[artifact("not-a-digest", 1, 1)],
                &[]
            )
            .is_err()
        );
        assert!(
            plan_dirty_overlay_recovery(&DirtyOverlayRecoveryState {
                checkout_id: "feature".into(),
                primary_overlay_digest: None,
                recovery_overlay_digest: None,
                dirty_paths: BTreeSet::from(["../other-worktree/file.rs".into()]),
            })
            .is_err()
        );
    }

    #[test]
    fn grace_boundary_is_reclaimable_and_zero_grace_does_not_hold_objects() {
        let plan = plan_cas_gc(
            100,
            CasGcPolicy {
                grace_period_secs: 10,
            },
            &[artifact(OLD, 90, 1)],
            &[],
        )
        .unwrap();
        assert_eq!(plan.sweep_candidates.len(), 1);

        let zero_grace = plan_cas_gc(
            100,
            CasGcPolicy {
                grace_period_secs: 0,
            },
            &[artifact(YOUNG, 100, 1)],
            &[],
        )
        .unwrap();
        assert_eq!(zero_grace.sweep_candidates.len(), 1);
    }
}
