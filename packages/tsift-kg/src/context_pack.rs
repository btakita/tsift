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
    let mut scan_truncated = false;
    for node in nodes {
        if node.kind != "semantic_entity" {
            continue;
        }
        if out.len() >= config.max_candidate_scan {
            scan_truncated = true;
            break;
        }
        out.push(node_to_candidate(node, degree.get(node.id.as_str()).copied().unwrap_or(0)));
    }
    (out, scan_truncated)
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
        .filter(|c| c.confidence >= config.min_confidence)
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
    ContextCandidate {
        node_id: node.id.clone(),
        label: node.label.clone(),
        kind,
        confidence,
        degree,
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
