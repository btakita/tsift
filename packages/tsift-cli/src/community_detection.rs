use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

use anyhow::Result;
use serde::{Deserialize, Serialize};
use tsift_graph as graph;
use tsift_index::index;
use tsift_search::tagpath_adapter;

use crate::output::tagpath::{CommunityMemberAmbiguityDiagnostic, TagpathAnnotationDiagnostic, TagpathSearchOpts};
use crate::{content_hash, GraphEffectivenessReadiness, hash_bytes_hex, shell_quote};

const COMMUNITY_DETECTION_CACHE_VERSION: &str = "community-detection-cache-v1";

static COMMUNITY_DETECTION_CACHE: OnceLock<Mutex<BTreeMap<String, graph::CommunityResult>>> =
    OnceLock::new();

#[derive(Debug, Clone, Serialize)]
pub(crate) struct CommunityDetectionDiagnostics {
    cache_hit: bool,
    edge_count: usize,
    iterations: usize,
    tagpath_state: String,
    tagpath_readiness: GraphEffectivenessReadiness,
    #[serde(skip_serializing_if = "Option::is_none")]
    tagpath_stale_reason: Option<String>,
    annotated_community_count: usize,
    annotated_member_count: usize,
    ambiguous_member_count: usize,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    ambiguous_members: Vec<CommunityMemberAmbiguityDiagnostic>,
}

#[derive(Debug, Clone)]
pub(crate) struct CommunityDetectionReport {
    pub(crate) result: graph::CommunityResult,
    pub(crate) diagnostics: CommunityDetectionDiagnostics,
}

#[derive(Debug, Clone)]
pub(crate) struct CommunityTagpathCachePart {
    pub(crate) state: String,
    pub(crate) reason: Option<String>,
    pub(crate) key: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct CommunityDetectionCacheEntry {
    version: String,
    key: String,
    result: graph::CommunityResult,
}

fn community_detection_cache() -> &'static Mutex<BTreeMap<String, graph::CommunityResult>> {
    COMMUNITY_DETECTION_CACHE.get_or_init(|| Mutex::new(BTreeMap::new()))
}

pub(crate) fn community_tagpath_cache_part_for_loaded(
    adapter: &tagpath_adapter::TagpathAdapter,
) -> CommunityTagpathCachePart {
    let index_path = tagpath::index::index_path(&adapter.project_root);
    let index_hash = fs::read(&index_path)
        .map(|bytes| hash_bytes_hex(&bytes))
        .unwrap_or_else(|err| hash_bytes_hex(format!("fresh-index-unreadable:{err:#}").as_bytes()));
    CommunityTagpathCachePart {
        state: "fresh".to_string(),
        reason: None,
        key: format!("fresh:{index_hash}"),
    }
}

pub(crate) fn community_tagpath_cache_part(
    root: &std::path::Path,
    opts: &TagpathSearchOpts,
) -> Result<CommunityTagpathCachePart> {
    if opts.no_tagpath {
        return Ok(CommunityTagpathCachePart {
            state: "disabled".to_string(),
            reason: None,
            key: "disabled".to_string(),
        });
    }
    match tagpath_adapter::try_load(root) {
        tagpath_adapter::LoadResult::Loaded(adapter) => {
            Ok(community_tagpath_cache_part_for_loaded(&adapter))
        }
        tagpath_adapter::LoadResult::Stale { reason, .. } => {
            if opts.strict {
                anyhow::bail!(
                    "tagpath index is stale (reason={reason}); rerun `tagpath index --update` or drop --tagpath-strict"
                );
            }
            Ok(CommunityTagpathCachePart {
                state: "stale".to_string(),
                key: format!("stale:{reason}"),
                reason: Some(reason),
            })
        }
        tagpath_adapter::LoadResult::Missing => Ok(CommunityTagpathCachePart {
            state: "missing".to_string(),
            reason: None,
            key: "missing".to_string(),
        }),
    }
}

pub(crate) fn graph_effectiveness_ready(reason: impl Into<String>) -> GraphEffectivenessReadiness {
    GraphEffectivenessReadiness {
        status: "ready".to_string(),
        fail_closed: false,
        reason: reason.into(),
        diagnostics: Vec::new(),
        next_commands: Vec::new(),
    }
}

pub(crate) fn graph_effectiveness_blocked(
    reason: impl Into<String>,
    diagnostics: Vec<String>,
    next_commands: Vec<String>,
) -> GraphEffectivenessReadiness {
    GraphEffectivenessReadiness {
        status: "blocked".to_string(),
        fail_closed: true,
        reason: reason.into(),
        diagnostics,
        next_commands,
    }
}

fn tagpath_index_update_command(root: &std::path::Path) -> String {
    format!(
        "cd {} && tagpath index --update",
        shell_quote(root.to_string_lossy().as_ref())
    )
}

fn graph_tagpath_readiness(
    root: &std::path::Path,
    tagpath: &CommunityTagpathCachePart,
) -> GraphEffectivenessReadiness {
    match tagpath.state.as_str() {
        "fresh" => graph_effectiveness_ready("tagpath_handles_available"),
        "disabled" => GraphEffectivenessReadiness {
            status: "disabled".to_string(),
            fail_closed: false,
            reason: "tagpath_lookup_disabled".to_string(),
            diagnostics: Vec::new(),
            next_commands: Vec::new(),
        },
        "stale" => graph_effectiveness_blocked(
            "tagpath_state_stale",
            vec![format!(
                "tagpath_state=stale{}: community members may miss stable tagpath_handle citations; rebuild the tagpath index before relying on handle coverage",
                tagpath
                    .reason
                    .as_ref()
                    .map(|reason| format!(" (reason={reason})"))
                    .unwrap_or_default()
            )],
            vec![tagpath_index_update_command(root)],
        ),
        "missing" => graph_effectiveness_blocked(
            "tagpath_state_missing",
            vec![format!(
                "tagpath_state=missing: community members cannot emit stable tagpath_handle citations; create .naming.toml if needed, then run tagpath indexing from {}",
                root.display()
            )],
            vec![tagpath_index_update_command(root)],
        ),
        state => graph_effectiveness_blocked(
            format!("tagpath_state_{state}"),
            vec![format!(
                "tagpath_state={state}: community tagpath_handle readiness is unknown"
            )],
            vec![tagpath_index_update_command(root)],
        ),
    }
}

fn community_graph_watermark(db: &index::IndexDb) -> Result<String> {
    let source_snapshot = db.source_snapshot_parts()?;
    let edge_rows = db.edge_count()?;
    let symbol_rows = db.symbol_count()?;
    content_hash(&serde_json::json!({
        "source_snapshot": source_snapshot,
        "edge_rows": edge_rows,
        "symbol_rows": symbol_rows,
    }))
}

fn community_detection_cache_key(
    root: &std::path::Path,
    scope: Option<&str>,
    graph_watermark: &str,
    tagpath: &CommunityTagpathCachePart,
) -> Result<String> {
    content_hash(&serde_json::json!({
        "version": COMMUNITY_DETECTION_CACHE_VERSION,
        "root": root.display().to_string(),
        "scope": scope.unwrap_or("root"),
        "graph_watermark": graph_watermark,
        "tagpath": tagpath.key,
    }))
}

fn community_detection_cache_path(
    root: &std::path::Path,
    scope: Option<&str>,
    key: &str,
) -> PathBuf {
    root.join(".tsift/community-cache")
        .join(scope.unwrap_or("root"))
        .join(format!("{key}.json"))
}

fn read_community_detection_cache(
    root: &std::path::Path,
    scope: Option<&str>,
    key: &str,
) -> Option<graph::CommunityResult> {
    let path = community_detection_cache_path(root, scope, key);
    let bytes = fs::read(path).ok()?;
    let entry: CommunityDetectionCacheEntry = serde_json::from_slice(&bytes).ok()?;
    if entry.version == COMMUNITY_DETECTION_CACHE_VERSION && entry.key == key {
        Some(entry.result)
    } else {
        None
    }
}

fn write_community_detection_cache(
    root: &std::path::Path,
    scope: Option<&str>,
    key: &str,
    result: &graph::CommunityResult,
) {
    let path = community_detection_cache_path(root, scope, key);
    let Some(parent) = path.parent() else {
        return;
    };
    if fs::create_dir_all(parent).is_err() {
        return;
    }
    let entry = CommunityDetectionCacheEntry {
        version: COMMUNITY_DETECTION_CACHE_VERSION.to_string(),
        key: key.to_string(),
        result: result.clone(),
    };
    if let Ok(bytes) = serde_json::to_vec(&entry) {
        let _ = fs::write(path, bytes);
    }
}

fn community_detection_diagnostics(
    cache_hit: bool,
    result: &graph::CommunityResult,
    tagpath: &CommunityTagpathCachePart,
    tagpath_root: &std::path::Path,
) -> CommunityDetectionDiagnostics {
    CommunityDetectionDiagnostics {
        cache_hit,
        edge_count: result.edge_count,
        iterations: result.iterations,
        tagpath_state: tagpath.state.clone(),
        tagpath_readiness: graph_tagpath_readiness(tagpath_root, tagpath),
        tagpath_stale_reason: tagpath.reason.clone(),
        annotated_community_count: 0,
        annotated_member_count: 0,
        ambiguous_member_count: 0,
        ambiguous_members: Vec::new(),
    }
}

pub(crate) fn update_community_annotation_diagnostics(
    diagnostics: &mut CommunityDetectionDiagnostics,
    communities: &[graph::Community],
    annotation: Option<&TagpathAnnotationDiagnostic>,
) {
    diagnostics.annotated_community_count = communities
        .iter()
        .filter(|community| {
            community
                .members
                .iter()
                .any(|member| member.tagpath_handle.is_some())
        })
        .count();
    diagnostics.annotated_member_count = communities
        .iter()
        .flat_map(|community| community.members.iter())
        .filter(|member| member.tagpath_handle.is_some())
        .count();
    if let Some(annotation) = annotation {
        diagnostics.ambiguous_member_count = annotation.ambiguous_members.len();
        diagnostics.ambiguous_members = annotation.ambiguous_members.clone();
    } else {
        diagnostics.ambiguous_member_count = 0;
        diagnostics.ambiguous_members.clear();
    }
}

pub(crate) fn detect_communities_cached(
    db: &index::IndexDb,
    root: &std::path::Path,
    scope: Option<&str>,
    tagpath: &CommunityTagpathCachePart,
    tagpath_root: &std::path::Path,
) -> Result<CommunityDetectionReport> {
    let graph_watermark = community_graph_watermark(db)?;
    let cache_key = community_detection_cache_key(root, scope, &graph_watermark, tagpath)?;

    if let Some(result) = community_detection_cache()
        .lock()
        .ok()
        .and_then(|cache| cache.get(&cache_key).cloned())
    {
        return Ok(CommunityDetectionReport {
            diagnostics: community_detection_diagnostics(true, &result, tagpath, tagpath_root),
            result,
        });
    }

    if let Some(result) = read_community_detection_cache(root, scope, &cache_key) {
        if let Ok(mut cache) = community_detection_cache().lock() {
            cache.insert(cache_key.clone(), result.clone());
        }
        return Ok(CommunityDetectionReport {
            diagnostics: community_detection_diagnostics(true, &result, tagpath, tagpath_root),
            result,
        });
    }

    let edges = db.all_edges()?;
    let result = graph::detect_communities(&edges);
    write_community_detection_cache(root, scope, &cache_key, &result);
    if let Ok(mut cache) = community_detection_cache().lock() {
        cache.insert(cache_key, result.clone());
    }
    Ok(CommunityDetectionReport {
        diagnostics: community_detection_diagnostics(false, &result, tagpath, tagpath_root),
        result,
    })
}

fn index_file_abs(file: &str, root: &std::path::Path) -> std::path::PathBuf {
    if std::path::Path::new(file).is_absolute() {
        std::path::PathBuf::from(file)
    } else {
        root.join(file)
    }
}

fn index_file_key(file: &str, root: &std::path::Path) -> String {
    let path = std::path::Path::new(file);
    let rel = if path.is_absolute() {
        path.strip_prefix(root).unwrap_or(path)
    } else {
        path
    };
    rel.to_string_lossy().replace('\\', "/")
}

fn tagpath_handle_for_index_file(
    file: &str,
    name: &str,
    root: &std::path::Path,
    adapter: &tagpath_adapter::TagpathAdapter,
) -> Option<String> {
    adapter.handle_for_member(&index_file_abs(file, root), name)
}

#[derive(Debug, Clone)]
struct TagpathHandleCandidate {
    file: String,
    line: i64,
    handle: String,
}

fn tagpath_handle_candidates_for_symbol_rows(
    name: &str,
    syms: &[index::StoredSymbol],
    root: &std::path::Path,
    adapter: &tagpath_adapter::TagpathAdapter,
) -> Vec<TagpathHandleCandidate> {
    syms.iter()
        .filter_map(|sym| {
            let handle = tagpath_handle_for_index_file(&sym.file, name, root, adapter)?;
            Some(TagpathHandleCandidate {
                file: index_file_key(&sym.file, root),
                line: sym.line,
                handle,
            })
        })
        .collect()
}

pub(crate) fn file_communities_from_callers(
    db: &index::IndexDb,
    root: &std::path::Path,
    scope: Option<&str>,
    tagpath: &CommunityTagpathCachePart,
) -> Result<std::collections::HashMap<String, std::collections::HashSet<usize>>> {
    let community_report = detect_communities_cached(db, root, scope, tagpath, root)?;
    if community_report.result.communities.is_empty() {
        return Ok(std::collections::HashMap::new());
    }
    let mut community_by_symbol = std::collections::HashMap::new();
    for community in community_report.result.communities {
        for member in community.members {
            community_by_symbol.insert(member.name, community.id);
        }
    }

    let mut communities_by_file: std::collections::HashMap<
        String,
        std::collections::HashSet<usize>,
    > = std::collections::HashMap::new();
    for sym in db.all_symbols()? {
        if let Some(community_id) = community_by_symbol.get(&sym.name) {
            communities_by_file
                .entry(index_file_key(&sym.file, root))
                .or_default()
                .insert(*community_id);
        }
    }
    for edge in db.all_stored_edges()? {
        if let Some(community_id) = community_by_symbol.get(&edge.caller_name) {
            communities_by_file
                .entry(index_file_key(&edge.caller_file, root))
                .or_default()
                .insert(*community_id);
        }
    }
    Ok(communities_by_file)
}

pub(crate) fn resolve_tagpath_handle_for_callee_edge(
    edge: &index::StoredEdge,
    db: &index::IndexDb,
    root: &std::path::Path,
    adapter: &tagpath_adapter::TagpathAdapter,
    communities_by_file: &std::collections::HashMap<String, std::collections::HashSet<usize>>,
) -> Option<String> {
    let syms = db.symbol_info(&edge.callee_name).ok()?;
    let candidates =
        tagpath_handle_candidates_for_symbol_rows(&edge.callee_name, &syms, root, adapter);
    let caller_file = index_file_key(&edge.caller_file, root);

    if let Some(candidate) = candidates
        .iter()
        .find(|candidate| candidate.file == caller_file)
    {
        return Some(candidate.handle.clone());
    }

    if let Some(caller_communities) = communities_by_file.get(&caller_file) {
        for candidate in &candidates {
            if let Some(candidate_communities) = communities_by_file.get(&candidate.file)
                && !caller_communities.is_disjoint(candidate_communities)
            {
                return Some(candidate.handle.clone());
            }
        }
    }

    candidates.first().map(|candidate| candidate.handle.clone())
}

fn push_bounded_community_member_ref(
    refs_by_member: &mut HashMap<(usize, String), Vec<graph::CommunityMemberRef>>,
    community_id: usize,
    name: &str,
    reference: graph::CommunityMemberRef,
) {
    let refs = refs_by_member
        .entry((community_id, name.to_string()))
        .or_default();
    if refs.iter().any(|existing| {
        existing.file == reference.file
            && existing.line == reference.line
            && existing.role == reference.role
            && existing.peer == reference.peer
    }) {
        return;
    }
    if refs.len() < 6 {
        refs.push(reference);
    }
}

fn choose_symbol_row_by_files<'a>(
    syms: &'a [index::StoredSymbol],
    files: &BTreeSet<String>,
    root: &std::path::Path,
) -> Option<(&'a index::StoredSymbol, &'static str)> {
    let matches: Vec<&index::StoredSymbol> = syms
        .iter()
        .filter(|sym| files.contains(&index_file_key(&sym.file, root)))
        .collect();
    if matches.len() == 1 {
        Some((matches[0], "edge_file"))
    } else {
        None
    }
}

fn choose_tagpath_candidate_by_files<'a>(
    candidates: &'a [TagpathHandleCandidate],
    files: &BTreeSet<String>,
    evidence: &'static str,
) -> Option<(&'a TagpathHandleCandidate, &'static str)> {
    let matches: Vec<&TagpathHandleCandidate> = candidates
        .iter()
        .filter(|candidate| files.contains(&candidate.file))
        .collect();
    if matches.len() == 1 {
        Some((matches[0], evidence))
    } else {
        None
    }
}

pub(crate) fn annotate_community_members_with_context(
    communities: &mut [graph::Community],
    db: &index::IndexDb,
    root: &std::path::Path,
    adapter: Option<&tagpath_adapter::TagpathAdapter>,
) -> Result<Vec<CommunityMemberAmbiguityDiagnostic>> {
    let mut community_by_name = HashMap::<String, usize>::new();
    for community in communities.iter() {
        for member in &community.members {
            community_by_name.insert(member.name.clone(), community.id);
        }
    }

    let mut symbols_by_name = HashMap::<String, Vec<index::StoredSymbol>>::new();
    for sym in db.all_symbols()? {
        symbols_by_name
            .entry(sym.name.clone())
            .or_default()
            .push(sym);
    }

    let mut refs_by_member = HashMap::<(usize, String), Vec<graph::CommunityMemberRef>>::new();
    let mut evidence_files_by_member = HashMap::<(usize, String), BTreeSet<String>>::new();
    let mut context_files_by_community = HashMap::<usize, BTreeSet<String>>::new();

    for edge in db.all_stored_edges()? {
        let Some(&caller_community) = community_by_name.get(&edge.caller_name) else {
            continue;
        };
        let Some(&callee_community) = community_by_name.get(&edge.callee_name) else {
            continue;
        };
        if caller_community != callee_community {
            continue;
        }

        let file = index_file_key(&edge.caller_file, root);
        context_files_by_community
            .entry(caller_community)
            .or_default()
            .insert(file.clone());

        evidence_files_by_member
            .entry((caller_community, edge.caller_name.clone()))
            .or_default()
            .insert(file.clone());
        push_bounded_community_member_ref(
            &mut refs_by_member,
            caller_community,
            &edge.caller_name,
            graph::CommunityMemberRef {
                file: file.clone(),
                line: edge.caller_line,
                role: "caller".to_string(),
                peer: edge.callee_name.clone(),
            },
        );

        evidence_files_by_member
            .entry((callee_community, edge.callee_name.clone()))
            .or_default()
            .insert(file.clone());
        push_bounded_community_member_ref(
            &mut refs_by_member,
            callee_community,
            &edge.callee_name,
            graph::CommunityMemberRef {
                file,
                line: edge.call_site_line,
                role: "callee".to_string(),
                peer: edge.caller_name.clone(),
            },
        );
    }

    let mut diagnostics = Vec::new();
    for community in communities.iter_mut() {
        let community_files = context_files_by_community
            .get(&community.id)
            .cloned()
            .unwrap_or_default();
        for member in community.members.iter_mut() {
            member.file = None;
            member.line = None;
            member.tagpath_handle = None;
            let key = (community.id, member.name.clone());
            member.refs = refs_by_member.remove(&key).unwrap_or_default();

            let syms = symbols_by_name
                .get(&member.name)
                .map(Vec::as_slice)
                .unwrap_or(&[]);
            let evidence_files = evidence_files_by_member
                .get(&key)
                .cloned()
                .unwrap_or_default();
            let candidates = adapter
                .map(|adapter| {
                    tagpath_handle_candidates_for_symbol_rows(&member.name, syms, root, adapter)
                })
                .unwrap_or_default();

            let mut selected_file: Option<String> = None;
            let mut selected_line: Option<i64> = None;
            let mut selected_handle: Option<String> = None;
            let mut selected_evidence: Option<&'static str> = None;

            if let Some(candidate) = candidates.first().filter(|_| candidates.len() == 1) {
                selected_file = Some(candidate.file.clone());
                selected_line = Some(candidate.line);
                selected_handle = Some(candidate.handle.clone());
                selected_evidence = Some("unique_tagpath_handle");
            } else if let Some((candidate, evidence)) =
                choose_tagpath_candidate_by_files(&candidates, &evidence_files, "edge_file")
            {
                selected_file = Some(candidate.file.clone());
                selected_line = Some(candidate.line);
                selected_handle = Some(candidate.handle.clone());
                selected_evidence = Some(evidence);
            } else if let Some((candidate, evidence)) =
                choose_tagpath_candidate_by_files(&candidates, &community_files, "community_file")
            {
                selected_file = Some(candidate.file.clone());
                selected_line = Some(candidate.line);
                selected_handle = Some(candidate.handle.clone());
                selected_evidence = Some(evidence);
            }

            if selected_file.is_none() {
                if let Some(sym) = syms.first().filter(|_| syms.len() == 1) {
                    selected_file = Some(index_file_key(&sym.file, root));
                    selected_line = Some(sym.line);
                    selected_evidence = Some("unique_symbol_row");
                } else if let Some((sym, evidence)) =
                    choose_symbol_row_by_files(syms, &evidence_files, root)
                {
                    selected_file = Some(index_file_key(&sym.file, root));
                    selected_line = Some(sym.line);
                    selected_evidence = Some(evidence);
                } else if let Some((sym, _)) =
                    choose_symbol_row_by_files(syms, &community_files, root)
                {
                    selected_file = Some(index_file_key(&sym.file, root));
                    selected_line = Some(sym.line);
                    selected_evidence = Some("community_file");
                }
            }

            member.file = selected_file.clone();
            member.line = selected_line;
            member.tagpath_handle = selected_handle;

            if syms.len() > 1 || candidates.len() > 1 {
                diagnostics.push(CommunityMemberAmbiguityDiagnostic {
                    community_id: community.id,
                    name: member.name.clone(),
                    candidate_count: syms.len(),
                    tagpath_candidate_count: candidates.len(),
                    evidence: selected_evidence
                        .unwrap_or("ambiguous_no_evidence")
                        .to_string(),
                    chosen_file: selected_file,
                });
            }
        }
    }

    Ok(diagnostics)
}
