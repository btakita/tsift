//! Phase 1 GraphRAG context retrieval (`#kgctxretrieve`): a deterministic,
//! bounded "known-entity pack" over a graph.db's `semantic_entity` nodes.
//!
//! Today [`crate::extract_documents_to_projection`] extracts each chunk in
//! isolation — the model re-invents `kgent-…` stable ids instead of reconciling
//! against entities already in the graph, producing duplicate/variant entities
//! and missed cross-chunk relations. This module builds the pack that Phase 2
//! (`#kgctxinject`) will inject into the extractor prompt so the model reuses
//! canonical entities.
//!
//! **Bounded + deterministic** (the Run Manifest contract): the candidate set is
//! capped by [`ContextPackConfig::max_candidate_scan`] (never a full 418k-node
//! dump — the same discipline as `#kgwiring`'s evidence cap), ranking has no
//! nondeterministic input, and ordering is fixed by seed match → connectivity →
//! confidence → stable node-id tiebreak. Pure + tested with a fixture graph.

use std::cmp::Ordering;
use std::collections::{BTreeSet, HashMap};

use serde::Serialize;
use tsift_core::{GraphEdge, GraphNode, GraphProjection};

use crate::KgChunk;

/// Configuration for context-pack retrieval. Every field bounds the result for
/// cost and determinism.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ContextPackConfig {
    /// Maximum known entities returned (top-K).
    pub max_entities: usize,
    /// Minimum entity confidence to include (`0.0` = no gate).
    pub min_confidence: f64,
    /// Maximum candidate nodes scanned from the graph (bounded-scan cap; never
    /// a full dump).
    pub max_candidate_scan: usize,
}

impl Default for ContextPackConfig {
    fn default() -> Self {
        Self {
            max_entities: 32,
            min_confidence: 0.0,
            max_candidate_scan: 5_000,
        }
    }
}

/// One bounded candidate entity derived from a `semantic_entity` graph node and
/// its connectivity. Pure input to [`build_context_pack`].
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ContextCandidate {
    pub node_id: String,
    pub label: String,
    pub kind: String,
    pub confidence: f64,
    pub degree: usize,
    /// #kgconfrank: `true` when `confidence` came from the extractor model
    /// (`confidence_source=model`). Model-sourced confidence ranks above a
    /// derived default at equal connectivity, so an unknown 0.500 default never
    /// outranks a real model score.
    pub confidence_is_model: bool,
    /// #kgconfgate: `true` only when `confidence_source` is explicitly `default`
    /// (a derived neutral default, not a measured score). A positive
    /// `min_confidence` gate excludes these, so an unknown default never survives
    /// a gate that would exclude a real low score. Untagged/legacy nodes (no
    /// `confidence_source`) are neither model nor default and keep raw gating.
    pub confidence_is_default: bool,
}

/// A ranked known entity in the resulting pack. `node_id` is the canonical
/// `kgent-…` stable id the extractor should reuse when its label/kind matches.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ContextEntity {
    pub node_id: String,
    pub label: String,
    pub kind: String,
    pub confidence: f64,
    pub degree: usize,
    /// Seed whose token overlap matched this entity, if any. `None` means it
    /// entered the pack on connectivity/confidence alone.
    pub matched_seed: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ContextPack {
    pub entities: Vec<ContextEntity>,
    /// True if the ranked candidate set exceeded `max_entities` (the pack was
    /// capped). Surfaced for diagnostics; never an error.
    pub truncated: bool,
}

impl ContextPack {
    pub fn is_empty(&self) -> bool {
        self.entities.is_empty()
    }
}

/// Collect bounded `semantic_entity` candidates from already-loaded graph nodes
/// and edges. Pure: the caller controls loading (a fixture graph for tests;
/// Phase 2 / the store path bounds the read). Scans at most
/// `config.max_candidate_scan` eligible nodes and computes each one's degree
/// (incident edge count) from a single edge pass. Nodes whose `confidence`
/// property is absent or unparseable default to `0.0` (still eligible, ranked
/// lower).
///
/// Returns `(candidates, scan_truncated)` where `scan_truncated` is `true` when
/// more eligible `semantic_entity` nodes existed beyond the cap.
pub fn collect_candidates_from_nodes(
    nodes: &[GraphNode],
    edges: &[GraphEdge],
    config: &ContextPackConfig,
) -> (Vec<ContextCandidate>, bool) {
    let mut degree: HashMap<&str, usize> = HashMap::new();
    for edge in edges {
        *degree.entry(edge.from_id.as_str()).or_insert(0) += 1;
        *degree.entry(edge.to_id.as_str()).or_insert(0) += 1;
    }

    let mut out: Vec<ContextCandidate> = Vec::new();
    // #kgentitycollapse: query-time identity merge. The extractor reconciles
    // recurring entities to a canonical `kgent-…` id (stored as the `entity_id`
    // property), but each chunk/source still projects its own distinct node id.
    // Collapsing candidates that share a canonical entity_id makes retrieval
    // surface ONE representative per logical entity (the graph keeps every
    // provenance-bearing node — this is a read-side merge, no deletion). Nodes
    // without a canonical id (label-local `e0`/`e1` slugs) stay distinct.
    let mut canonical_slot: HashMap<String, usize> = HashMap::new();
    let mut scanned = 0usize;
    let mut scan_truncated = false;
    for node in nodes {
        if node.kind != "semantic_entity" {
            continue;
        }
        if scanned >= config.max_candidate_scan {
            scan_truncated = true;
            break;
        }
        scanned += 1;
        let cand = node_to_candidate(node, degree.get(node.id.as_str()).copied().unwrap_or(0));
        match canonical_entity_id(node) {
            Some(canon) => match canonical_slot.get(&canon).copied() {
                Some(idx) => {
                    // Keep the strongest representative but never understate the
                    // logical entity's connectivity — carry the max degree.
                    let merged_degree = out[idx].degree.max(cand.degree);
                    if candidate_supersedes(&cand, &out[idx]) {
                        out[idx] = cand;
                    }
                    out[idx].degree = merged_degree;
                }
                None => {
                    canonical_slot.insert(canon, out.len());
                    out.push(cand);
                }
            },
            None => out.push(cand),
        }
    }
    (out, scan_truncated)
}

/// #kgconfrank: ranking tier for a candidate's confidence provenance — `0` for a
/// real model score (ranks first), `1` for a derived default. Used as a sort key
/// ahead of raw confidence so defaults never outrank model scores.
fn confidence_tier(cand: &ContextCandidate) -> u8 {
    if cand.confidence_is_model { 0 } else { 1 }
}

/// #kgconfgate: provenance-aware confidence gate. A positive `min_confidence`
/// excludes explicit derived defaults outright — an unknown default is not a
/// measured score, so it must not survive a gate that would drop a real low
/// score. Model-sourced and untagged/legacy nodes gate by raw confidence; when
/// `min_confidence` is `0.0` (no gate) everything passes as before.
fn passes_confidence_gate(cand: &ContextCandidate, min_confidence: f64) -> bool {
    if cand.confidence_is_default && min_confidence > 0.0 {
        return false;
    }
    cand.confidence >= min_confidence
}

/// The canonical `kgent-…` identity an extractor reconciled this node to, if any.
/// Only canonical ids (not the model's chunk-local `e0`/`e1` slugs) are merge
/// keys, so distinct entities that happen to share a local slug stay separate.
fn canonical_entity_id(node: &GraphNode) -> Option<String> {
    node.properties
        .get("entity_id")
        .filter(|id| id.starts_with("kgent-"))
        .cloned()
}

/// Total order for choosing the representative of a merged canonical group:
/// higher confidence, then higher degree, then smaller node id (deterministic).
fn candidate_supersedes(new: &ContextCandidate, current: &ContextCandidate) -> bool {
    (
        new.confidence,
        new.degree,
        std::cmp::Reverse(new.node_id.as_str()),
    )
        .partial_cmp(&(
            current.confidence,
            current.degree,
            std::cmp::Reverse(current.node_id.as_str()),
        ))
        .map(|ord| ord == Ordering::Greater)
        .unwrap_or(false)
}

/// Collect candidates from a [`GraphProjection`] — the natural integration
/// point (fixture tests and the extractor both produce one).
pub fn collect_candidates_from_projection(
    projection: &GraphProjection,
    config: &ContextPackConfig,
) -> (Vec<ContextCandidate>, bool) {
    collect_candidates_from_nodes(&projection.nodes, &projection.edges, config)
}

/// Pure, deterministic context-pack ranker.
///
/// Ranks the bounded candidate set by:
/// 1. **Seed match** — case-insensitive token overlap between a seed phrase and
///    the entity label. Matched entities rank above unmatched ones.
/// 2. **Connectivity** — incident edge count (degree), descending.
/// 3. **Confidence**, descending.
/// 4. **Stable node-id tiebreak**, ascending — full determinism.
///
/// Confidence-gated by `config.min_confidence` and capped to
/// `config.max_entities`. Determinism holds because every ordering input is
/// totally ordered (seed order, usize degree, bounded confidence, node id).
pub fn build_context_pack(
    seeds: &[String],
    candidates: &[ContextCandidate],
    config: &ContextPackConfig,
) -> ContextPack {
    let seed_tokens: Vec<BTreeSet<String>> = seeds.iter().map(|s| tokenize(s)).collect();
    let mut ranked: Vec<Ranked> = candidates
        .iter()
        .filter(|c| passes_confidence_gate(c, config.min_confidence))
        .map(|c| {
            let matched_seed = best_seed_match(&c.label, &seed_tokens, seeds);
            let match_rank = if matched_seed.is_some() { 0u8 } else { 1u8 };
            Ranked {
                cand: c,
                matched_seed,
                match_rank,
            }
        })
        .collect();
    ranked.sort_by(|a, b| {
        a.match_rank
            .cmp(&b.match_rank)
            .then_with(|| b.cand.degree.cmp(&a.cand.degree))
            // #kgconfrank: model-sourced confidence ranks above a derived default
            // at equal connectivity, so an unknown 0.500 default never outranks a
            // real model score (e.g. model 0.300 beats default 0.500).
            .then_with(|| confidence_tier(a.cand).cmp(&confidence_tier(b.cand)))
            .then_with(|| {
                b.cand
                    .confidence
                    .partial_cmp(&a.cand.confidence)
                    .unwrap_or(Ordering::Equal)
            })
            .then_with(|| a.cand.node_id.cmp(&b.cand.node_id))
    });

    let truncated = ranked.len() > config.max_entities;
    let entities = ranked
        .into_iter()
        .take(config.max_entities)
        .map(|r| ContextEntity {
            node_id: r.cand.node_id.clone(),
            label: r.cand.label.clone(),
            kind: r.cand.kind.clone(),
            confidence: r.cand.confidence,
            degree: r.cand.degree,
            matched_seed: r.matched_seed,
        })
        .collect();
    ContextPack {
        entities,
        truncated,
    }
}

/// Build a context pack straight from a [`GraphProjection`]: collect the bounded
/// candidate set, then rank it.
pub fn build_context_pack_from_projection(
    seeds: &[String],
    projection: &GraphProjection,
    config: &ContextPackConfig,
) -> ContextPack {
    let (candidates, _) = collect_candidates_from_projection(projection, config);
    build_context_pack(seeds, &candidates, config)
}

/// Derive salient seed phrases from a chunk of text (`#kgctxinject`). Deterministic:
/// alphanumeric tokens of length > 2, lower-cased and de-duplicated, ordered by
/// first appearance, capped at `max_seeds`. These seeds drive the per-chunk
/// known-entity retrieval so the extractor can reconcile against existing graph
/// entities rather than re-inventing them.
pub fn derive_seeds(chunk_text: &str, max_seeds: usize) -> Vec<String> {
    let mut seen: BTreeSet<String> = BTreeSet::new();
    let mut seeds: Vec<String> = Vec::new();
    for token in chunk_text.split(|c: char| !c.is_alphanumeric()) {
        if token.len() <= 2 {
            continue;
        }
        let lower = token.to_lowercase();
        if seen.insert(lower.clone()) {
            seeds.push(lower);
            if seeds.len() >= max_seeds {
                break;
            }
        }
    }
    seeds
}

/// Bounded per-chunk context source for graph-aware extraction (`#kgctxinject`).
/// For each chunk it derives seeds from the chunk text, then builds a ranked
/// known-entity pack from the supplied projection. Cheap to construct; the
/// expensive work is bounded by the embedded [`ContextPackConfig`].
pub struct ChunkContextSource<'a> {
    pub projection: &'a GraphProjection,
    pub config: ContextPackConfig,
    pub max_seeds: usize,
}

impl<'a> ChunkContextSource<'a> {
    pub fn new(projection: &'a GraphProjection, config: ContextPackConfig) -> Self {
        Self {
            projection,
            config,
            max_seeds: 16,
        }
    }

    /// Override the default seed budget (16) for per-chunk seed derivation.
    pub fn with_max_seeds(mut self, max_seeds: usize) -> Self {
        self.max_seeds = max_seeds;
        self
    }

    /// Build the known-entity pack for one chunk — deterministic and bounded.
    pub fn context_for_chunk(&self, chunk: &KgChunk) -> ContextPack {
        let seeds = derive_seeds(&chunk.text, self.max_seeds);
        build_context_pack_from_projection(&seeds, self.projection, &self.config)
    }
}

struct Ranked<'a> {
    cand: &'a ContextCandidate,
    matched_seed: Option<String>,
    match_rank: u8,
}

fn node_to_candidate(node: &GraphNode, degree: usize) -> ContextCandidate {
    let confidence = node
        .properties
        .get("confidence")
        .and_then(|c| c.parse::<f64>().ok())
        .unwrap_or(0.0);
    let kind = node
        .properties
        .get("entity_kind")
        .cloned()
        .unwrap_or_else(|| node.kind.clone());
    // #kgconfrank / #kgconfgate: distinguish model-sourced, explicit-default, and
    // untagged (legacy) confidence provenance.
    let confidence_source = node.properties.get("confidence_source");
    let confidence_is_model = confidence_source.is_some_and(|source| source == "model");
    let confidence_is_default = confidence_source.is_some_and(|source| source == "default");
    ContextCandidate {
        node_id: node.id.clone(),
        label: node.label.clone(),
        kind,
        confidence,
        degree,
        confidence_is_model,
        confidence_is_default,
    }
}

/// Lowercase alphanumeric token set for deterministic overlap matching.
fn tokenize(text: &str) -> BTreeSet<String> {
    text.split(|c: char| !c.is_alphanumeric())
        .filter(|s| s.len() > 1)
        .map(|s| s.to_lowercase())
        .collect()
}

/// First seed (in input order) whose token set overlaps the label's tokens, for
/// deterministic, order-stable matching.
fn best_seed_match(
    label: &str,
    seed_tokens: &[BTreeSet<String>],
    seeds: &[String],
) -> Option<String> {
    let label_tokens = tokenize(label);
    for (i, tokens) in seed_tokens.iter().enumerate() {
        if !tokens.is_disjoint(&label_tokens) {
            return Some(seeds[i].clone());
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ent(id: &str, label: &str, kind: &str, confidence: f64) -> GraphNode {
        GraphNode::new(id, "semantic_entity", label)
            .with_property("entity_kind", kind)
            .with_property("confidence", format!("{confidence:.3}"))
    }

    #[test]
    fn seed_match_ranks_above_higher_connectivity() {
        // a: high degree + high confidence, but no seed match.
        // b: seed match, low degree + low confidence.
        let mut p = GraphProjection::default();
        p.nodes.push(ent("kgent-a", "OllamaKgExtractor", "type", 0.9));
        p.nodes.push(ent("kgent-b", "GraphProjection", "type", 0.5));
        for i in 0..5 {
            p.edges.push(GraphEdge::new("kgent-a", format!("x-{i}"), "related"));
        }
        let cfg = ContextPackConfig {
            max_entities: 2,
            ..Default::default()
        };
        let pack = build_context_pack_from_projection(&["GraphProjection".to_string()], &p, &cfg);
        assert_eq!(pack.entities[0].node_id, "kgent-b");
        assert_eq!(
            pack.entities[0].matched_seed.as_deref(),
            Some("GraphProjection")
        );
        assert_eq!(pack.entities[1].node_id, "kgent-a");
        assert!(pack.entities[1].matched_seed.is_none());
    }

    #[test]
    fn deterministic_tiebreak_by_node_id() {
        let mut p = GraphProjection::default();
        p.nodes.push(ent("kgent-z", "Zed Entity", "type", 0.5));
        p.nodes.push(ent("kgent-a", "Aye Entity", "type", 0.5));
        let cfg = ContextPackConfig {
            max_entities: 2,
            ..Default::default()
        };
        let pack = build_context_pack_from_projection(
            &["nomatch-seed".to_string()],
            &p,
            &cfg,
        );
        // No seed overlap; equal degree/confidence → stable node-id order.
        assert_eq!(pack.entities[0].node_id, "kgent-a");
        assert_eq!(pack.entities[1].node_id, "kgent-z");
    }

    #[test]
    fn confidence_gate_excludes_low_confidence_entities() {
        let mut p = GraphProjection::default();
        p.nodes.push(ent("kgent-a", "Alpha", "type", 0.3));
        p.nodes.push(ent("kgent-b", "Beta", "type", 0.8));
        let cfg = ContextPackConfig {
            max_entities: 5,
            min_confidence: 0.5,
            ..Default::default()
        };
        let pack = build_context_pack_from_projection(&[], &p, &cfg);
        assert_eq!(pack.entities.len(), 1);
        assert_eq!(pack.entities[0].node_id, "kgent-b");
    }

    /// #kgconfgate: a positive min_confidence gate excludes an explicit derived
    /// default (even a high 0.9) but admits a real model score at/above the gate;
    /// untagged/legacy nodes keep raw-confidence gating.
    #[test]
    fn confidence_gate_is_provenance_aware() {
        let mut p = GraphProjection::default();
        p.nodes.push(
            ent("kgent-default-hi", "DefaultHigh", "type", 0.9)
                .with_property("confidence_source", "default"),
        );
        p.nodes.push(
            ent("kgent-model-mid", "ModelMid", "type", 0.5)
                .with_property("confidence_source", "model"),
        );
        // Untagged/legacy node (no confidence_source) keeps raw gating.
        p.nodes.push(ent("kgent-legacy", "Legacy", "type", 0.7));
        let cfg = ContextPackConfig {
            max_entities: 10,
            min_confidence: 0.4,
            ..Default::default()
        };
        let pack = build_context_pack_from_projection(&[], &p, &cfg);
        let ids: Vec<&str> = pack.entities.iter().map(|e| e.node_id.as_str()).collect();
        assert!(ids.contains(&"kgent-model-mid"), "model 0.5 passes the 0.4 gate");
        assert!(
            ids.contains(&"kgent-legacy"),
            "untagged legacy 0.7 keeps raw gating"
        );
        assert!(
            !ids.contains(&"kgent-default-hi"),
            "explicit derived default excluded by a positive gate despite 0.9"
        );
    }

    /// #kgconfgate: with no gate (`min_confidence == 0`) derived defaults still
    /// pass — the exclusion only applies to a positive threshold.
    #[test]
    fn confidence_gate_admits_defaults_when_threshold_is_zero() {
        let mut p = GraphProjection::default();
        p.nodes.push(
            ent("kgent-default", "Def", "type", 0.5).with_property("confidence_source", "default"),
        );
        let pack = build_context_pack_from_projection(&[], &p, &ContextPackConfig::default());
        assert_eq!(pack.entities.len(), 1);
    }

    #[test]
    fn max_entities_caps_and_flags_truncated() {
        let mut p = GraphProjection::default();
        for i in 0..10 {
            p.nodes.push(ent(
                &format!("kgent-{i}"),
                &format!("Entity {i}"),
                "type",
                0.5,
            ));
        }
        let cfg = ContextPackConfig {
            max_entities: 3,
            ..Default::default()
        };
        let pack = build_context_pack_from_projection(&[], &p, &cfg);
        assert_eq!(pack.entities.len(), 3);
        assert!(pack.truncated);
    }

    #[test]
    fn bounded_candidate_scan_truncates_and_caps() {
        let mut p = GraphProjection::default();
        for i in 0..100 {
            p.nodes.push(ent(
                &format!("kgent-{i}"),
                &format!("Entity {i}"),
                "type",
                0.5,
            ));
        }
        let cfg = ContextPackConfig {
            max_candidate_scan: 5,
            max_entities: 100,
            ..Default::default()
        };
        let (cands, scan_truncated) = collect_candidates_from_projection(&p, &cfg);
        assert_eq!(cands.len(), 5);
        assert!(scan_truncated);
    }

    #[test]
    fn ignores_non_semantic_entity_nodes() {
        let mut p = GraphProjection::default();
        p.nodes.push(ent("kgent-a", "Alpha", "type", 0.5));
        p.nodes.push(
            GraphNode::new("ast-1", "function", "some_function")
                .with_property("entity_kind", "function"),
        );
        let (cands, _) = collect_candidates_from_projection(&p, &ContextPackConfig::default());
        assert_eq!(cands.len(), 1);
        assert_eq!(cands[0].node_id, "kgent-a");
    }

    /// #kgentitycollapse: candidates reconciled to the same canonical entity_id
    /// collapse to one representative (highest confidence, max degree); distinct
    /// canonical ids and local-slug ids stay separate.
    #[test]
    fn collapses_candidates_sharing_canonical_entity_id() {
        let mut p = GraphProjection::default();
        p.nodes.push(
            ent("kgent-chunkA", "GraphProjection", "type", 0.4)
                .with_property("entity_id", "kgent-canon"),
        );
        p.nodes.push(
            ent("kgent-chunkB", "GraphProjection", "type", 0.8)
                .with_property("entity_id", "kgent-canon"),
        );
        p.nodes.push(
            ent("kgent-chunkC", "SqliteGraphStore", "type", 0.6)
                .with_property("entity_id", "kgent-other"),
        );
        // A local-slug id ("e0") is NOT a merge key — stays distinct.
        p.nodes.push(
            ent("kgent-chunkD", "GraphProjection", "type", 0.9).with_property("entity_id", "e0"),
        );
        // chunkA has higher degree; the merged representative must carry it.
        p.edges.push(GraphEdge::new("kgent-chunkA", "x", "related"));
        p.edges.push(GraphEdge::new("kgent-chunkA", "y", "related"));

        let (cands, _) = collect_candidates_from_projection(&p, &ContextPackConfig::default());
        assert_eq!(cands.len(), 3, "canon merged + other + local-slug = 3");
        let canon = cands
            .iter()
            .find(|c| c.node_id == "kgent-chunkB")
            .expect("higher-confidence node is the representative");
        assert_eq!(canon.confidence, 0.8);
        assert_eq!(canon.degree, 2, "representative carries the group's max degree");
        assert!(
            cands.iter().all(|c| c.node_id != "kgent-chunkA"),
            "merged-away node id is gone"
        );
    }

    #[test]
    fn empty_seeds_rank_by_connectivity_then_confidence() {
        let mut p = GraphProjection::default();
        p.nodes.push(ent("kgent-a", "Alpha", "type", 0.5));
        p.nodes.push(ent("kgent-b", "Beta", "type", 0.9));
        // Give `a` strictly higher degree so it outranks `b` despite lower conf.
        p.edges.push(GraphEdge::new("kgent-a", "kgent-b", "related"));
        p.edges.push(GraphEdge::new("kgent-a", "x", "related"));
        let cfg = ContextPackConfig {
            max_entities: 2,
            ..Default::default()
        };
        let pack = build_context_pack_from_projection(&[], &p, &cfg);
        assert_eq!(pack.entities[0].node_id, "kgent-a");
        assert!(pack.entities[0].matched_seed.is_none());
    }

    /// #kgconfrank: a real model score (0.3) ranks above a higher *derived
    /// default* (0.5) at equal connectivity — the default must not outrank it.
    #[test]
    fn model_confidence_outranks_derived_default() {
        let mut p = GraphProjection::default();
        p.nodes.push(
            ent("kgent-model", "ModelScored", "type", 0.3)
                .with_property("confidence_source", "model"),
        );
        p.nodes.push(
            ent("kgent-default", "DefaultScored", "type", 0.5)
                .with_property("confidence_source", "default"),
        );
        let cfg = ContextPackConfig {
            max_entities: 2,
            ..Default::default()
        };
        let pack = build_context_pack_from_projection(&[], &p, &cfg);
        assert_eq!(
            pack.entities[0].node_id, "kgent-model",
            "model score ranks first despite lower raw confidence"
        );
        assert_eq!(pack.entities[1].node_id, "kgent-default");
    }

    /// #kgconfrank: within the same provenance tier, higher raw confidence still
    /// wins (the tier only breaks model-vs-default ties, not model-vs-model).
    #[test]
    fn within_model_tier_higher_confidence_wins() {
        let mut p = GraphProjection::default();
        p.nodes.push(
            ent("kgent-lo", "Lo", "type", 0.4).with_property("confidence_source", "model"),
        );
        p.nodes.push(
            ent("kgent-hi", "Hi", "type", 0.9).with_property("confidence_source", "model"),
        );
        let cfg = ContextPackConfig {
            max_entities: 2,
            ..Default::default()
        };
        let pack = build_context_pack_from_projection(&[], &p, &cfg);
        assert_eq!(pack.entities[0].node_id, "kgent-hi");
        assert_eq!(pack.entities[1].node_id, "kgent-lo");
    }

    #[test]
    fn candidate_defaults_confidence_to_zero_when_property_absent() {
        let node = GraphNode::new("kgent-x", "semantic_entity", "NoConf")
            .with_property("entity_kind", "type");
        let cand = node_to_candidate(&node, 3);
        assert_eq!(cand.confidence, 0.0);
        assert_eq!(cand.degree, 3);
        assert_eq!(cand.kind, "type");
    }

    fn chunk(text: &str) -> KgChunk {
        KgChunk {
            id: "chunk-0".to_string(),
            document_id: "doc-0".to_string(),
            kind: crate::KgInputKind::Source,
            source_ref: "test.md".to_string(),
            ordinal: 0,
            byte_start: 0,
            byte_end: text.len(),
            text: text.to_string(),
        }
    }

    #[test]
    fn derive_seeds_is_deterministic_lowercased_deduped_and_capped() {
        // First-appearance order, lower-cased, deduped, tokens of length > 2,
        // capped at max_seeds.
        let seeds = derive_seeds("OllamaKgExtractor parses Ollama JSON; ok ok ok", 16);
        assert_eq!(seeds, vec!["ollamakgextractor", "parses", "ollama", "json"]);
        // "ok" is length 2 → skipped; duplicate "ok" never appears.
        assert!(!seeds.iter().any(|s| s == "ok"));

        let capped = derive_seeds("alpha beta gamma delta", 2);
        assert_eq!(capped, vec!["alpha", "beta"]);
    }

    #[test]
    fn chunk_context_source_retrieves_seed_matched_entities() {
        // The chunk mentions "GraphProjection"; the source should surface that
        // entity (seed match) ahead of an unrelated higher-degree entity.
        let mut p = GraphProjection::default();
        p.nodes.push(ent("kgent-gp", "GraphProjection", "type", 0.5));
        p.nodes.push(ent("kgent-other", "UnrelatedThing", "type", 0.9));
        for i in 0..5 {
            p.edges
                .push(GraphEdge::new("kgent-other", format!("x-{i}"), "related"));
        }
        let cfg = ContextPackConfig {
            max_entities: 2,
            ..Default::default()
        };
        let source = ChunkContextSource::new(&p, cfg);
        let pack = source.context_for_chunk(&chunk("This chunk discusses the GraphProjection type."));
        assert_eq!(pack.entities[0].node_id, "kgent-gp");
        assert_eq!(
            pack.entities[0].matched_seed.as_deref(),
            Some("graphprojection")
        );
    }

    #[test]
    fn chunk_context_source_honors_max_seeds_override() {
        let p = GraphProjection::default();
        let source = ChunkContextSource::new(&p, ContextPackConfig::default()).with_max_seeds(3);
        assert_eq!(source.max_seeds, 3);
        // Empty graph yields an empty pack regardless of seeds.
        let pack = source.context_for_chunk(&chunk("alpha beta gamma delta"));
        assert!(pack.is_empty());
    }
}
