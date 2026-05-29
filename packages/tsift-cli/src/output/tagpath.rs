use anyhow::Result;
use serde::Serialize;

use crate::{
    EdgeSide, annotate_community_members_with_context, community_tagpath_cache_part_for_loaded,
    file_communities_from_callers, resolve_tagpath_handle_for_callee_edge,
};


/// Diagnostic produced by [`annotate_hits_with_tagpath`].
#[derive(Debug, Default, Clone)]
pub struct TagpathAnnotationDiagnostic {
    pub stale: bool,
    pub reason: Option<String>,
    pub loaded: bool,
    pub ambiguous_members: Vec<CommunityMemberAmbiguityDiagnostic>,
}

#[derive(Debug, Clone, Serialize)]
pub struct CommunityMemberAmbiguityDiagnostic {
    pub community_id: usize,
    pub name: String,
    pub candidate_count: usize,
    pub tagpath_candidate_count: usize,
    pub evidence: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub chosen_file: Option<String>,
}

#[allow(clippy::too_many_arguments)]
#[allow(dead_code)]
/// Tagpath consumer-side options for the search command.
#[derive(Debug, Default, Clone, Copy)]
pub struct TagpathSearchOpts {
    /// Skip tagpath lookup entirely (do not annotate hits).
    pub no_tagpath: bool,
    /// Treat a stale tagpath index as a hard error instead of falling back.
    pub strict: bool,
}

/// Annotate symbol hits in-place with `tagpath_handle` when a fresh tagpath
/// index is present at `root`. Returns a diagnostic describing whether the
/// adapter loaded, missed, or fell back due to staleness.
///
/// Behavior matrix:
///
/// | `no_tagpath` | index state | strict | action |
/// |---|---|---|---|
/// | true        | any         | any     | no-op |
/// | false       | missing     | any     | no-op |
/// | false       | fresh       | any     | annotate hits |
/// | false       | stale       | false   | emit diagnostic, no annotation |
/// | false       | stale       | true    | return error |
pub fn annotate_hits_with_tagpath(
    hits: &mut [tsift::index::SymbolHit],
    root: &std::path::Path,
    opts: &TagpathSearchOpts,
) -> Result<TagpathAnnotationDiagnostic> {
    if opts.no_tagpath {
        return Ok(TagpathAnnotationDiagnostic::default());
    }
    match tsift::tagpath_adapter::try_load(root) {
        tsift::tagpath_adapter::LoadResult::Loaded(adapter) => {
            for hit in hits.iter_mut() {
                let abs = if std::path::Path::new(&hit.file).is_absolute() {
                    std::path::PathBuf::from(&hit.file)
                } else {
                    root.join(&hit.file)
                };
                if let Some(handle) = adapter.handle_for_member(&abs, &hit.name) {
                    hit.tagpath_handle = Some(handle);
                }
            }
            Ok(TagpathAnnotationDiagnostic {
                stale: false,
                reason: None,
                loaded: true,
                ambiguous_members: Vec::new(),
            })
        }
        tsift::tagpath_adapter::LoadResult::Stale { reason, .. } => {
            if opts.strict {
                anyhow::bail!(
                    "tagpath index is stale (reason={reason}); rerun `tagpath index --update` or drop --tagpath-strict"
                );
            }
            Ok(TagpathAnnotationDiagnostic {
                stale: true,
                reason: Some(reason),
                loaded: false,
                ambiguous_members: Vec::new(),
            })
        }
        tsift::tagpath_adapter::LoadResult::Missing => Ok(TagpathAnnotationDiagnostic::default()),
    }
}

/// Annotate `StoredSymbol` definitions in-place with `tagpath_handle` when
/// a fresh tagpath index is present at `root`. Same fallback matrix as
/// [`annotate_hits_with_tagpath`].
pub fn annotate_stored_symbols_with_tagpath(
    symbols: &mut [tsift::index::StoredSymbol],
    root: &std::path::Path,
    opts: &TagpathSearchOpts,
) -> Result<TagpathAnnotationDiagnostic> {
    if opts.no_tagpath {
        return Ok(TagpathAnnotationDiagnostic::default());
    }
    match tsift::tagpath_adapter::try_load(root) {
        tsift::tagpath_adapter::LoadResult::Loaded(adapter) => {
            for sym in symbols.iter_mut() {
                let abs = if std::path::Path::new(&sym.file).is_absolute() {
                    std::path::PathBuf::from(&sym.file)
                } else {
                    root.join(&sym.file)
                };
                if let Some(handle) = adapter.handle_for_member(&abs, &sym.name) {
                    sym.tagpath_handle = Some(handle);
                }
            }
            Ok(TagpathAnnotationDiagnostic {
                stale: false,
                reason: None,
                loaded: true,
                ambiguous_members: Vec::new(),
            })
        }
        tsift::tagpath_adapter::LoadResult::Stale { reason, .. } => {
            if opts.strict {
                anyhow::bail!(
                    "tagpath index is stale (reason={reason}); rerun `tagpath index --update` or drop --tagpath-strict"
                );
            }
            Ok(TagpathAnnotationDiagnostic {
                stale: true,
                reason: Some(reason),
                loaded: false,
                ambiguous_members: Vec::new(),
            })
        }
        tsift::tagpath_adapter::LoadResult::Missing => Ok(TagpathAnnotationDiagnostic::default()),
    }
}

/// Annotate `StoredEdge` rows in-place with `tagpath_handle` from the
/// tagpath index. The handle names whichever endpoint the consumer cares
/// about: pass `EdgeSide::Caller` for caller rows (uses `caller_file` +
/// `caller_name` directly) and `EdgeSide::Callee` for callee rows
/// (resolves each callee edge via `db.symbol_info`, preferring the
/// caller's file or community before the no-context first-handle fallback).
pub fn annotate_stored_edges_with_tagpath(
    edges: &mut [tsift::index::StoredEdge],
    db: &tsift::index::IndexDb,
    root: &std::path::Path,
    scope: Option<&str>,
    side: EdgeSide,
    opts: &TagpathSearchOpts,
) -> Result<TagpathAnnotationDiagnostic> {
    if opts.no_tagpath {
        return Ok(TagpathAnnotationDiagnostic::default());
    }
    match tsift::tagpath_adapter::try_load(root) {
        tsift::tagpath_adapter::LoadResult::Loaded(adapter) => {
            match side {
                EdgeSide::Caller => {
                    for edge in edges.iter_mut() {
                        let abs = if std::path::Path::new(&edge.caller_file).is_absolute() {
                            std::path::PathBuf::from(&edge.caller_file)
                        } else {
                            root.join(&edge.caller_file)
                        };
                        if let Some(handle) = adapter.handle_for_member(&abs, &edge.caller_name) {
                            edge.tagpath_handle = Some(handle);
                        }
                    }
                }
                EdgeSide::Callee => {
                    let tagpath = community_tagpath_cache_part_for_loaded(&adapter);
                    let communities_by_file =
                        file_communities_from_callers(db, root, scope, &tagpath)
                            .unwrap_or_default();
                    for edge in edges.iter_mut() {
                        if let Some(handle) = resolve_tagpath_handle_for_callee_edge(
                            edge,
                            db,
                            root,
                            &adapter,
                            &communities_by_file,
                        ) {
                            edge.tagpath_handle = Some(handle);
                        }
                    }
                }
            }
            Ok(TagpathAnnotationDiagnostic {
                stale: false,
                reason: None,
                loaded: true,
                ambiguous_members: Vec::new(),
            })
        }
        tsift::tagpath_adapter::LoadResult::Stale { reason, .. } => {
            if opts.strict {
                anyhow::bail!(
                    "tagpath index is stale (reason={reason}); rerun `tagpath index --update` or drop --tagpath-strict"
                );
            }
            Ok(TagpathAnnotationDiagnostic {
                stale: true,
                reason: Some(reason),
                loaded: false,
                ambiguous_members: Vec::new(),
            })
        }
        tsift::tagpath_adapter::LoadResult::Missing => Ok(TagpathAnnotationDiagnostic::default()),
    }
}

/// Annotate every `tsift::graph::CommunityMember` in `communities` in-place with
/// file/ref context and, when available, the stable `mem:` handle from the
/// tagpath index. Duplicate names are resolved through edge/community
/// evidence rather than a name-only first-row fallback.
pub fn annotate_communities_with_tagpath(
    communities: &mut [tsift::graph::Community],
    db: &tsift::index::IndexDb,
    root: &std::path::Path,
    opts: &TagpathSearchOpts,
) -> Result<Option<TagpathAnnotationDiagnostic>> {
    if opts.no_tagpath {
        annotate_community_members_with_context(communities, db, root, None)?;
        return Ok(None);
    }
    match tsift::tagpath_adapter::try_load(root) {
        tsift::tagpath_adapter::LoadResult::Loaded(adapter) => {
            let ambiguous_members =
                annotate_community_members_with_context(communities, db, root, Some(&adapter))?;
            Ok(Some(TagpathAnnotationDiagnostic {
                stale: false,
                reason: None,
                loaded: true,
                ambiguous_members,
            }))
        }
        tsift::tagpath_adapter::LoadResult::Stale { reason, .. } => {
            if opts.strict {
                anyhow::bail!(
                    "tagpath index is stale (reason={reason}); rerun `tagpath index --update` or drop --tagpath-strict"
                );
            }
            let ambiguous_members =
                annotate_community_members_with_context(communities, db, root, None)?;
            Ok(Some(TagpathAnnotationDiagnostic {
                stale: true,
                reason: Some(reason),
                loaded: false,
                ambiguous_members,
            }))
        }
        tsift::tagpath_adapter::LoadResult::Missing => {
            annotate_community_members_with_context(communities, db, root, None)?;
            Ok(None)
        }
    }
}

/// Annotate `tsift::graph::PathNode` entries in-place with `tagpath_handle` when a
/// fresh tagpath index is present at `root`. Symbol→file resolution goes
/// through `db.symbol_info(name)` (first definition wins), then the adapter
/// resolves the per-member handle. Same fallback matrix as
/// [`annotate_hits_with_tagpath`].
pub fn annotate_path_nodes_with_tagpath(
    nodes: &mut [tsift::graph::PathNode],
    db: &tsift::index::IndexDb,
    root: &std::path::Path,
    opts: &TagpathSearchOpts,
) -> Result<TagpathAnnotationDiagnostic> {
    if opts.no_tagpath {
        return Ok(TagpathAnnotationDiagnostic::default());
    }
    match tsift::tagpath_adapter::try_load(root) {
        tsift::tagpath_adapter::LoadResult::Loaded(adapter) => {
            for node in nodes.iter_mut() {
                let syms = match db.symbol_info(&node.name) {
                    Ok(v) => v,
                    Err(_) => continue,
                };
                let Some(sym) = syms.into_iter().next() else {
                    continue;
                };
                let abs = if std::path::Path::new(&sym.file).is_absolute() {
                    std::path::PathBuf::from(&sym.file)
                } else {
                    root.join(&sym.file)
                };
                if let Some(handle) = adapter.handle_for_member(&abs, &node.name) {
                    node.tagpath_handle = Some(handle);
                }
            }
            Ok(TagpathAnnotationDiagnostic {
                stale: false,
                reason: None,
                loaded: true,
                ambiguous_members: Vec::new(),
            })
        }
        tsift::tagpath_adapter::LoadResult::Stale { reason, .. } => {
            if opts.strict {
                anyhow::bail!(
                    "tagpath index is stale (reason={reason}); rerun `tagpath index --update` or drop --tagpath-strict"
                );
            }
            Ok(TagpathAnnotationDiagnostic {
                stale: true,
                reason: Some(reason),
                loaded: false,
                ambiguous_members: Vec::new(),
            })
        }
        tsift::tagpath_adapter::LoadResult::Missing => Ok(TagpathAnnotationDiagnostic::default()),
    }
}
