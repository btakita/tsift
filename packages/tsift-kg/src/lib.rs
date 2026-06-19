use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;
use tsift_core::{GraphEdge, GraphFreshness, GraphNode, GraphProjection, GraphProvenance};
use tsift_local_model::ProviderKind;
use tsift_sqlite::SqliteGraphStore;

pub mod context_pack;
pub mod ollama;

pub use ollama::OllamaKgExtractor;

pub const KG_CONTRACT_VERSION: &str = "tsift-kg-v1";
pub const HASH_KG_EXTRACTOR_ID: &str = "tsift-local-hash-v1";
/// `provider` property stamped on every KG-projected node/edge. Used to scope
/// per-source replacement (#kgrefreshdup) so it never touches non-KG graph rows.
pub const KG_PROVIDER: &str = "tsift-kg";
/// #kgsameas: edge kind linking duplicate `semantic_entity` nodes that share a
/// canonical `entity_id`, so graph-level consumers collapse them to one logical
/// entity (the durable counterpart to the context-pack query-time merge).
pub const KG_SAME_AS_EDGE: &str = "same_as";

/// #kgconf: neutral fallback confidence used when an extractor does not emit a
/// per-entity/relation confidence. The projection always persists a `confidence`
/// property (tagged `confidence_source=default` here, `=model` when the value
/// came from the extractor) so the confidence/recency gating (#kgctxinject) has
/// data to act on instead of silently treating every entity as `0.0`.
pub const DEFAULT_KG_CONFIDENCE: f64 = 0.5;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum KgInputKind {
    Source,
    Session,
    Memory,
}

impl KgInputKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Source => "source",
            Self::Session => "session",
            Self::Memory => "memory",
        }
    }

    fn document_node_kind(self) -> &'static str {
        match self {
            Self::Source => "kg_source",
            Self::Session => "kg_session",
            Self::Memory => "kg_memory",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KgInputDocument {
    pub id: String,
    pub kind: KgInputKind,
    pub source_ref: String,
    pub text: String,
}

impl KgInputDocument {
    pub fn new(kind: KgInputKind, source_ref: impl Into<String>, text: impl Into<String>) -> Self {
        let source_ref = source_ref.into();
        let text = text.into();
        let id = stable_id("kgdoc", &[kind.as_str(), &source_ref]);
        Self {
            id,
            kind,
            source_ref,
            text,
        }
    }

    pub fn source(source_ref: impl Into<String>, text: impl Into<String>) -> Self {
        Self::new(KgInputKind::Source, source_ref, text)
    }

    pub fn session(source_ref: impl Into<String>, text: impl Into<String>) -> Self {
        Self::new(KgInputKind::Session, source_ref, text)
    }

    pub fn memory(source_ref: impl Into<String>, text: impl Into<String>) -> Self {
        Self::new(KgInputKind::Memory, source_ref, text)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ChunkingConfig {
    pub max_chars: usize,
    pub overlap_chars: usize,
}

impl Default for ChunkingConfig {
    fn default() -> Self {
        Self {
            max_chars: 6_000,
            overlap_chars: 400,
        }
    }
}

impl ChunkingConfig {
    fn validate(self) -> Result<()> {
        if self.max_chars == 0 {
            bail!("chunk max_chars must be greater than zero");
        }
        if self.overlap_chars >= self.max_chars {
            bail!("chunk overlap_chars must be smaller than max_chars");
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KgChunk {
    pub id: String,
    pub document_id: String,
    pub kind: KgInputKind,
    pub source_ref: String,
    pub ordinal: usize,
    pub byte_start: usize,
    pub byte_end: usize,
    pub text: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KgExtractorMetadata {
    pub provider_id: String,
    pub provider_kind: ProviderKind,
    pub extraction_model: String,
}

impl KgExtractorMetadata {
    pub fn hash_fallback() -> Self {
        Self {
            provider_id: HASH_KG_EXTRACTOR_ID.to_string(),
            provider_kind: ProviderKind::HashFallback,
            extraction_model: HASH_KG_EXTRACTOR_ID.to_string(),
        }
    }
}

pub trait KgExtractor {
    fn metadata(&self) -> KgExtractorMetadata;
    fn extract_json(&self, chunk: &KgChunk) -> Result<String>;

    /// Context-aware extraction (`#kgctxinject`). The default ignores the
    /// context pack and delegates to [`extract_json`](Self::extract_json), so
    /// every existing extractor (e.g. `HashKgExtractor`) is backward-compatible.
    /// `OllamaKgExtractor` overrides it to inject the known-entity pack into the
    /// prompt so the model reconciles against canonical stable ids.
    fn extract_json_with_context(
        &self,
        chunk: &KgChunk,
        context: Option<&context_pack::ContextPack>,
    ) -> Result<String> {
        let _ = context;
        self.extract_json(chunk)
    }
}

#[derive(Debug, Default, Clone, Copy)]
pub struct HashKgExtractor;

impl KgExtractor for HashKgExtractor {
    fn metadata(&self) -> KgExtractorMetadata {
        KgExtractorMetadata::hash_fallback()
    }

    fn extract_json(&self, chunk: &KgChunk) -> Result<String> {
        let mut entities = Vec::new();
        for (index, token) in kg_tokens(&chunk.text).into_iter().take(8).enumerate() {
            entities.push(KgJsonEntity {
                id: format!("e{index}"),
                label: token.clone(),
                kind: "concept".to_string(),
                description: Some(format!("hash fallback concept `{token}`")),
                confidence: Some(0.5),
            });
        }
        if entities.is_empty() {
            entities.push(KgJsonEntity {
                id: "e0".to_string(),
                label: "text".to_string(),
                kind: "concept".to_string(),
                description: Some("hash fallback empty-token concept".to_string()),
                confidence: Some(0.1),
            });
        }

        let mut relations = Vec::new();
        for pair in entities.windows(2) {
            relations.push(KgJsonRelation {
                from: pair[0].id.clone(),
                to: pair[1].id.clone(),
                kind: "related_to".to_string(),
                label: Some("hash fallback adjacency".to_string()),
                confidence: Some(0.25),
            });
        }

        serde_json::to_string(&KgExtractionPayload {
            entities,
            relations,
        })
        .map_err(Into::into)
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct KgJsonEntity {
    pub id: String,
    pub label: String,
    pub kind: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub confidence: Option<f64>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct KgJsonRelation {
    pub from: String,
    pub to: String,
    pub kind: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub confidence: Option<f64>,
}

#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct KgExtractionPayload {
    #[serde(default)]
    pub entities: Vec<KgJsonEntity>,
    #[serde(default)]
    pub relations: Vec<KgJsonRelation>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct KgExtractedChunk {
    pub chunk: KgChunk,
    pub raw_json: String,
    pub payload: KgExtractionPayload,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct KgBackendCandidate {
    pub backend: String,
    pub ready: bool,
    pub operation: String,
    pub notes: String,
}

#[derive(Debug, Clone, PartialEq)]
pub struct KgExtractionReport {
    pub chunks: Vec<KgChunk>,
    pub extracted_chunks: Vec<KgExtractedChunk>,
    pub projection: GraphProjection,
    pub backend_candidates: Vec<KgBackendCandidate>,
    pub diagnostics: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct KgSqliteUpsertReport {
    pub graph_db: String,
    pub nodes_upserted: usize,
    pub edges_upserted: usize,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct KgMultiRunVerificationReport {
    pub first_node_count: usize,
    pub second_node_count: usize,
    pub first_edge_count: usize,
    pub second_edge_count: usize,
    pub stable_node_ids: usize,
    pub stable_edge_ids: usize,
    pub duplicate_node_ids: Vec<String>,
    pub duplicate_edge_ids: Vec<String>,
}

pub fn chunk_documents(
    documents: &[KgInputDocument],
    config: ChunkingConfig,
) -> Result<Vec<KgChunk>> {
    config.validate()?;
    let mut chunks = Vec::new();
    for document in documents {
        chunks.extend(chunk_document(document, config)?);
    }
    Ok(chunks)
}

pub fn parse_and_validate_extraction(raw_json: &str) -> Result<KgExtractionPayload> {
    let payload: KgExtractionPayload =
        serde_json::from_str(raw_json).context("parsing KG extraction JSON")?;
    validate_extraction(&payload)?;
    Ok(payload)
}

pub fn validate_extraction(payload: &KgExtractionPayload) -> Result<()> {
    let mut entity_ids = BTreeSet::new();
    for entity in &payload.entities {
        validate_non_empty("entity.id", &entity.id)?;
        validate_non_empty("entity.label", &entity.label)?;
        validate_non_empty("entity.kind", &entity.kind)?;
        validate_confidence("entity.confidence", entity.confidence)?;
        if !entity_ids.insert(entity.id.clone()) {
            bail!("duplicate entity id `{}` in KG extraction", entity.id);
        }
    }

    for relation in &payload.relations {
        validate_non_empty("relation.from", &relation.from)?;
        validate_non_empty("relation.to", &relation.to)?;
        validate_non_empty("relation.kind", &relation.kind)?;
        validate_confidence("relation.confidence", relation.confidence)?;
        if !entity_ids.contains(&relation.from) {
            bail!(
                "relation `{}` -> `{}` references unknown from entity `{}`",
                relation.from,
                relation.to,
                relation.from
            );
        }
        if !entity_ids.contains(&relation.to) {
            bail!(
                "relation `{}` -> `{}` references unknown to entity `{}`",
                relation.from,
                relation.to,
                relation.to
            );
        }
    }

    Ok(())
}

pub fn extract_documents_to_projection<E: KgExtractor + ?Sized>(
    documents: &[KgInputDocument],
    extractor: &E,
    config: ChunkingConfig,
) -> Result<KgExtractionReport> {
    extract_documents_to_projection_with_context(documents, extractor, config, None)
}

/// Context-aware extraction (`#kgctxinject`). Like
/// [`extract_documents_to_projection`], but when a [`ChunkContextSource`] is
/// supplied each chunk's known-entity pack is built from the existing graph and
/// passed to [`KgExtractor::extract_json_with_context`] so the model reuses
/// canonical `kgent-…` stable ids instead of re-inventing them. `None` behaves
/// exactly like the plain path.
pub fn extract_documents_to_projection_with_context<E: KgExtractor + ?Sized>(
    documents: &[KgInputDocument],
    extractor: &E,
    config: ChunkingConfig,
    context_source: Option<&context_pack::ChunkContextSource<'_>>,
) -> Result<KgExtractionReport> {
    let chunks = chunk_documents(documents, config)?;
    let mut extracted_chunks = Vec::new();
    for chunk in &chunks {
        let pack = context_source.map(|src| src.context_for_chunk(chunk));
        let raw_json = extractor
            .extract_json_with_context(chunk, pack.as_ref())
            .with_context(|| format!("extracting KG JSON for chunk {}", chunk.id))?;
        let payload = parse_and_validate_extraction(&raw_json)
            .with_context(|| format!("validating KG JSON for chunk {}", chunk.id))?;
        extracted_chunks.push(KgExtractedChunk {
            chunk: chunk.clone(),
            raw_json,
            payload,
        });
    }

    let metadata = extractor.metadata();
    let projection = projection_from_extractions(documents, &extracted_chunks, &metadata)?;
    Ok(KgExtractionReport {
        chunks,
        extracted_chunks,
        projection,
        backend_candidates: default_backend_candidates(),
        diagnostics: Vec::new(),
    })
}

pub fn upsert_kg_projection_sqlite(
    graph_db: &Path,
    projection: &GraphProjection,
) -> Result<KgSqliteUpsertReport> {
    let mut store = SqliteGraphStore::open(graph_db)?;
    store.upsert_projection(projection)?;
    Ok(KgSqliteUpsertReport {
        graph_db: graph_db.display().to_string(),
        nodes_upserted: projection.nodes.len(),
        edges_upserted: projection.edges.len(),
    })
}

/// #kgrefreshdup: replace each source's KG subgraph instead of accumulating it.
///
/// `upsert_kg_projection_sqlite` is purely additive — re-extracting an edited
/// source shifts chunk byte-ranges (so chunk + entity node ids change) and the
/// non-deterministic model may relabel entities, leaving the prior nodes orphaned
/// while new ones pile on (observed 9 → 18 entities for one source after a single
/// `refresh`). This variant first deletes every prior KG node projected from each
/// `source_ref` present in `projection` (scoped to the `tsift-kg` provider so it
/// never touches AST nodes), then upserts the fresh projection — making
/// per-source extraction idempotent. Distinct sources are untouched.
pub fn replace_kg_source_projection_sqlite(
    graph_db: &Path,
    projection: &GraphProjection,
) -> Result<KgSqliteUpsertReport> {
    let mut store = SqliteGraphStore::open(graph_db)?;
    for source_ref in projection_source_refs(projection) {
        store.delete_source_projection(&source_ref, KG_PROVIDER)?;
    }
    store.upsert_projection(projection)?;
    Ok(KgSqliteUpsertReport {
        graph_db: graph_db.display().to_string(),
        nodes_upserted: projection.nodes.len(),
        edges_upserted: projection.edges.len(),
    })
}

/// #kgsameas: link canonical-entity duplicates across the whole graph with
/// durable `same_as` edges so graph-level consumers (graph/explain/summarize,
/// SurrealDB) see one logical entity — the durable counterpart to the
/// context-pack query-time merge (#kgentitycollapse). Groups `semantic_entity`
/// nodes by canonical `entity_id` (`kgent-` prefixed) and stars each group's
/// members to its smallest node id. No node is deleted (provenance preserved);
/// idempotent. Returns the number of `same_as` edges written.
pub fn link_canonical_entities_sqlite(graph_db: &Path) -> Result<usize> {
    let mut store = SqliteGraphStore::open(graph_db)?;
    store.link_nodes_by_shared_property(
        "semantic_entity",
        "entity_id",
        "kgent-",
        KG_SAME_AS_EDGE,
        &[("provider", KG_PROVIDER), ("contract", KG_CONTRACT_VERSION)],
    )
}

/// Distinct `source_ref` property values across the projection's nodes, in
/// stable sorted order. Used to scope per-source replacement.
fn projection_source_refs(projection: &GraphProjection) -> BTreeSet<String> {
    projection
        .nodes
        .iter()
        .filter_map(|node| node.properties.get("source_ref").cloned())
        .collect()
}

pub fn verify_projection_multi_run_stability(
    first: &GraphProjection,
    second: &GraphProjection,
) -> Result<KgMultiRunVerificationReport> {
    let first_node_ids = projection_node_ids(first);
    let second_node_ids = projection_node_ids(second);
    let first_edge_ids = projection_edge_ids(first);
    let second_edge_ids = projection_edge_ids(second);
    let duplicate_node_ids = duplicate_node_ids(first)
        .into_iter()
        .chain(duplicate_node_ids(second))
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect::<Vec<_>>();
    let duplicate_edge_ids = duplicate_edge_ids(first)
        .into_iter()
        .chain(duplicate_edge_ids(second))
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect::<Vec<_>>();

    if !duplicate_node_ids.is_empty() {
        bail!(
            "KG projection contains duplicate node ids: {}",
            duplicate_node_ids.join(", ")
        );
    }
    if !duplicate_edge_ids.is_empty() {
        bail!(
            "KG projection contains duplicate edge ids: {}",
            duplicate_edge_ids.join(", ")
        );
    }
    if first_node_ids != second_node_ids {
        bail!("KG projection node ids changed across identical runs");
    }
    if first_edge_ids != second_edge_ids {
        bail!("KG projection edge ids changed across identical runs");
    }

    Ok(KgMultiRunVerificationReport {
        first_node_count: first.nodes.len(),
        second_node_count: second.nodes.len(),
        first_edge_count: first.edges.len(),
        second_edge_count: second.edges.len(),
        stable_node_ids: first_node_ids.len(),
        stable_edge_ids: first_edge_ids.len(),
        duplicate_node_ids,
        duplicate_edge_ids,
    })
}

pub fn default_backend_candidates() -> Vec<KgBackendCandidate> {
    vec![KgBackendCandidate {
        backend: "sqlite".to_string(),
        ready: true,
        operation: "upsert_kg_projection_sqlite".to_string(),
        notes: "writes provider-neutral GraphProjection rows into .tsift/graph.db".to_string(),
    }]
}

pub fn projection_from_extractions(
    documents: &[KgInputDocument],
    extracted_chunks: &[KgExtractedChunk],
    metadata: &KgExtractorMetadata,
) -> Result<GraphProjection> {
    let mut projection = GraphProjection::default();
    let mut known_documents = BTreeMap::new();
    for document in documents {
        known_documents.insert(document.id.clone(), document.clone());
        projection
            .nodes
            .push(node_with_content_freshness(document_node(
                document, metadata,
            ))?);
    }

    for extracted in extracted_chunks {
        let Some(document) = known_documents.get(&extracted.chunk.document_id) else {
            bail!(
                "chunk {} references unknown document {}",
                extracted.chunk.id,
                extracted.chunk.document_id
            );
        };
        let provenance = kg_provenance(&extracted.chunk, &extracted.raw_json);
        projection
            .nodes
            .push(node_with_content_freshness(chunk_node(
                &extracted.chunk,
                metadata,
            ))?);
        projection.edges.push(edge_with_content_freshness(
            GraphEdge::new(
                document.id.clone(),
                extracted.chunk.id.clone(),
                "contains_chunk",
            )
            .with_property("provider", "tsift-kg")
            .with_property("input_kind", extracted.chunk.kind.as_str())
            .with_property("chunk_ordinal", extracted.chunk.ordinal.to_string())
            .with_provenance(provenance.clone()),
        )?);

        let mut entity_nodes_by_local_id = BTreeMap::new();
        for entity in &extracted.payload.entities {
            let node_id = stable_id("kgent", &[&extracted.chunk.id, &entity.id]);
            entity_nodes_by_local_id.insert(entity.id.clone(), node_id.clone());
            projection.nodes.push(node_with_content_freshness(
                entity_node(&node_id, entity, &extracted.chunk, metadata)
                    .with_provenance(provenance.clone()),
            )?);
            projection.edges.push(edge_with_content_freshness(
                GraphEdge::new(extracted.chunk.id.clone(), node_id, "mentions_entity")
                    .with_property("provider", "tsift-kg")
                    .with_property("entity_id", entity.id.clone())
                    .with_property("entity_kind", entity.kind.clone())
                    .with_provenance(provenance.clone()),
            )?);
        }

        for relation in &extracted.payload.relations {
            let from_id = entity_nodes_by_local_id
                .get(&relation.from)
                .context("validated relation missing from entity map")?;
            let to_id = entity_nodes_by_local_id
                .get(&relation.to)
                .context("validated relation missing to entity map")?;
            projection.edges.push(edge_with_content_freshness(
                relation_edge(from_id, to_id, relation, &extracted.chunk, metadata)
                    .with_provenance(provenance.clone()),
            )?);
        }
    }

    Ok(projection)
}

fn chunk_document(document: &KgInputDocument, config: ChunkingConfig) -> Result<Vec<KgChunk>> {
    config.validate()?;
    if document.text.trim().is_empty() {
        return Ok(Vec::new());
    }

    let mut boundaries: Vec<usize> = document
        .text
        .char_indices()
        .map(|(index, _)| index)
        .collect();
    boundaries.push(document.text.len());
    let char_count = boundaries.len().saturating_sub(1);

    let mut chunks = Vec::new();
    let mut start_char = 0;
    let mut ordinal = 0;
    while start_char < char_count {
        let end_char = (start_char + config.max_chars).min(char_count);
        let byte_start = boundaries[start_char];
        let byte_end = boundaries[end_char];
        let text = document.text[byte_start..byte_end].trim().to_string();
        if !text.is_empty() {
            let id = stable_id(
                "kgchunk",
                &[&document.id, &ordinal.to_string(), &byte_start.to_string()],
            );
            chunks.push(KgChunk {
                id,
                document_id: document.id.clone(),
                kind: document.kind,
                source_ref: document.source_ref.clone(),
                ordinal,
                byte_start,
                byte_end,
                text,
            });
            ordinal += 1;
        }
        if end_char == char_count {
            break;
        }
        let next_start = end_char.saturating_sub(config.overlap_chars);
        start_char = if next_start <= start_char {
            end_char
        } else {
            next_start
        };
    }

    Ok(chunks)
}

fn document_node(document: &KgInputDocument, metadata: &KgExtractorMetadata) -> GraphNode {
    GraphNode::new(
        document.id.clone(),
        document.kind.document_node_kind(),
        truncate_label(&document.source_ref),
    )
    .with_property("provider", "tsift-kg")
    .with_property("contract", KG_CONTRACT_VERSION)
    .with_property("input_kind", document.kind.as_str())
    .with_property("source_ref", document.source_ref.clone())
    // #kgextractrefresh: record the source content hash so `kg refresh` can
    // detect when a source file changed since it was last extracted.
    .with_property(
        "source_content_hash",
        blake3::hash(document.text.as_bytes()).to_hex().to_string(),
    )
    .with_property("kg_provider", metadata.provider_id.clone())
    .with_property(
        "kg_provider_kind",
        provider_kind_name(&metadata.provider_kind),
    )
    .with_property("kg_extraction_model", metadata.extraction_model.clone())
    .with_provenance(GraphProvenance::new("tsift-kg", &document.source_ref))
}

fn chunk_node(chunk: &KgChunk, metadata: &KgExtractorMetadata) -> GraphNode {
    GraphNode::new(
        chunk.id.clone(),
        "kg_chunk",
        format!("{} chunk {}", chunk.kind.as_str(), chunk.ordinal),
    )
    .with_property("provider", "tsift-kg")
    .with_property("contract", KG_CONTRACT_VERSION)
    .with_property("input_kind", chunk.kind.as_str())
    .with_property("source_ref", chunk.source_ref.clone())
    .with_property("document_id", chunk.document_id.clone())
    .with_property("chunk_ordinal", chunk.ordinal.to_string())
    .with_property("byte_start", chunk.byte_start.to_string())
    .with_property("byte_end", chunk.byte_end.to_string())
    .with_property("text_preview", truncate_label(&chunk.text))
    .with_property("kg_provider", metadata.provider_id.clone())
    .with_property(
        "kg_provider_kind",
        provider_kind_name(&metadata.provider_kind),
    )
    .with_property("kg_extraction_model", metadata.extraction_model.clone())
    .with_provenance(kg_provenance(chunk, &chunk.text))
}

fn entity_node(
    node_id: &str,
    entity: &KgJsonEntity,
    chunk: &KgChunk,
    metadata: &KgExtractorMetadata,
) -> GraphNode {
    let mut node = GraphNode::new(node_id, "semantic_entity", entity.label.clone())
        .with_property("provider", "tsift-kg")
        .with_property("contract", KG_CONTRACT_VERSION)
        .with_property("source_ref", chunk.source_ref.clone())
        .with_property("document_id", chunk.document_id.clone())
        .with_property("chunk_id", chunk.id.clone())
        .with_property("entity_id", entity.id.clone())
        .with_property("entity_kind", entity.kind.clone())
        .with_property("kg_provider", metadata.provider_id.clone())
        .with_property(
            "kg_provider_kind",
            provider_kind_name(&metadata.provider_kind),
        )
        .with_property("kg_extraction_model", metadata.extraction_model.clone());
    if let Some(description) = &entity.description {
        node = node.with_property("description", description.clone());
    }
    // #kgconf: always persist a confidence score (model value when present, else a
    // derived neutral default) plus its source, so downstream gating never sees a
    // missing confidence and can weight model-emitted values above defaults.
    let (confidence, confidence_source) = match entity.confidence {
        Some(value) => (value, "model"),
        None => (DEFAULT_KG_CONFIDENCE, "default"),
    };
    node = node
        .with_property("confidence", format!("{confidence:.3}"))
        .with_property("confidence_source", confidence_source);
    node
}

fn relation_edge(
    from_id: &str,
    to_id: &str,
    relation: &KgJsonRelation,
    chunk: &KgChunk,
    metadata: &KgExtractorMetadata,
) -> GraphEdge {
    let mut edge = GraphEdge::new(from_id, to_id, "semantic_relation")
        .with_property("provider", "tsift-kg")
        .with_property("contract", KG_CONTRACT_VERSION)
        .with_property("source_ref", chunk.source_ref.clone())
        .with_property("document_id", chunk.document_id.clone())
        .with_property("chunk_id", chunk.id.clone())
        .with_property("relation_kind", relation.kind.clone())
        .with_property("kg_provider", metadata.provider_id.clone())
        .with_property(
            "kg_provider_kind",
            provider_kind_name(&metadata.provider_kind),
        )
        .with_property("kg_extraction_model", metadata.extraction_model.clone());
    if let Some(label) = &relation.label {
        edge = edge.with_property("label", label.clone());
    }
    // #kgconf: persist confidence + source on every relation edge, mirroring the
    // entity-node guarantee so relation gating is operable too.
    let (confidence, confidence_source) = match relation.confidence {
        Some(value) => (value, "model"),
        None => (DEFAULT_KG_CONFIDENCE, "default"),
    };
    edge = edge
        .with_property("confidence", format!("{confidence:.3}"))
        .with_property("confidence_source", confidence_source);
    edge
}

fn validate_non_empty(field: &str, value: &str) -> Result<()> {
    if value.trim().is_empty() {
        bail!("{field} must not be empty");
    }
    Ok(())
}

fn validate_confidence(field: &str, confidence: Option<f64>) -> Result<()> {
    let Some(confidence) = confidence else {
        return Ok(());
    };
    if !(0.0..=1.0).contains(&confidence) {
        bail!("{field} must be between 0.0 and 1.0");
    }
    Ok(())
}

fn stable_id(prefix: &str, parts: &[&str]) -> String {
    let mut hasher = blake3::Hasher::new();
    hasher.update(prefix.as_bytes());
    for part in parts {
        hasher.update(&[0]);
        hasher.update(part.as_bytes());
    }
    let digest = hasher.finalize().to_hex();
    format!("{prefix}-{}", &digest[..16])
}

fn content_hash<T: Serialize>(value: &T) -> Result<String> {
    let bytes = serde_json::to_vec(value)?;
    Ok(blake3::hash(&bytes).to_hex().to_string())
}

fn node_with_content_freshness(mut node: GraphNode) -> Result<GraphNode> {
    let mut hashable = node.clone();
    hashable.freshness = None;
    node.freshness = Some(GraphFreshness::content_hash(content_hash(&hashable)?));
    Ok(node)
}

fn edge_with_content_freshness(mut edge: GraphEdge) -> Result<GraphEdge> {
    let mut hashable = edge.clone();
    hashable.freshness = None;
    edge.freshness = Some(GraphFreshness::content_hash(content_hash(&hashable)?));
    Ok(edge)
}

fn projection_node_ids(projection: &GraphProjection) -> BTreeSet<String> {
    projection
        .nodes
        .iter()
        .map(|node| node.id.clone())
        .collect()
}

fn projection_edge_ids(projection: &GraphProjection) -> BTreeSet<String> {
    projection
        .edges
        .iter()
        .map(|edge| edge.id.clone())
        .collect()
}

fn duplicate_node_ids(projection: &GraphProjection) -> Vec<String> {
    duplicate_ids(projection.nodes.iter().map(|node| node.id.as_str()))
}

fn duplicate_edge_ids(projection: &GraphProjection) -> Vec<String> {
    duplicate_ids(projection.edges.iter().map(|edge| edge.id.as_str()))
}

fn duplicate_ids<'a>(ids: impl IntoIterator<Item = &'a str>) -> Vec<String> {
    let mut seen = BTreeSet::new();
    let mut duplicates = BTreeSet::new();
    for id in ids {
        if !seen.insert(id.to_string()) {
            duplicates.insert(id.to_string());
        }
    }
    duplicates.into_iter().collect()
}

fn kg_provenance(chunk: &KgChunk, raw: &str) -> GraphProvenance {
    GraphProvenance::new("tsift-kg", &chunk.source_ref)
        .with_content_hash(blake3::hash(raw.as_bytes()).to_hex().to_string())
}

fn provider_kind_name(provider_kind: &ProviderKind) -> &'static str {
    match provider_kind {
        ProviderKind::LlamaCpp => "llama.cpp",
        ProviderKind::Ollama => "ollama",
        ProviderKind::Vllm => "vllm",
        ProviderKind::HashFallback => "hash_fallback",
    }
}

fn truncate_label(input: &str) -> String {
    let trimmed = input.trim();
    let count = trimmed.chars().count();
    if count <= 120 {
        return trimmed.to_string();
    }
    let prefix: String = trimmed.chars().take(117).collect();
    format!("{prefix}...")
}

fn kg_tokens(input: &str) -> BTreeSet<String> {
    input
        .split(|ch: char| !(ch.is_ascii_alphanumeric() || ch == '_' || ch == '-'))
        .flat_map(|part| part.split(['_', '-']))
        .map(str::trim)
        .filter(|part| part.len() >= 3)
        .map(|part| part.to_ascii_lowercase())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use rusqlite::Connection;
    use tempfile::TempDir;

    #[derive(Debug)]
    struct FixtureExtractor;

    impl KgExtractor for FixtureExtractor {
        fn metadata(&self) -> KgExtractorMetadata {
            KgExtractorMetadata {
                provider_id: "fixture-provider".to_string(),
                provider_kind: ProviderKind::LlamaCpp,
                extraction_model: "fixture-model".to_string(),
            }
        }

        fn extract_json(&self, _chunk: &KgChunk) -> Result<String> {
            Ok(serde_json::json!({
                "entities": [
                    {"id": "a", "label": "GraphProjection", "kind": "type", "confidence": 0.9},
                    {"id": "b", "label": "SQLite upsert", "kind": "operation", "confidence": 0.8}
                ],
                "relations": [
                    {"from": "a", "to": "b", "kind": "materializes", "label": "materializes rows", "confidence": 0.7}
                ]
            })
            .to_string())
        }
    }

    #[test]
    fn chunk_documents_covers_source_session_and_memory_inputs() {
        let docs = vec![
            KgInputDocument::source("src/lib.rs", "abcdef ghijkl mnopqr"),
            KgInputDocument::session("tasks/session.md", "session text"),
            KgInputDocument::memory("memory:1", "memory text"),
        ];
        let chunks = chunk_documents(
            &docs,
            ChunkingConfig {
                max_chars: 8,
                overlap_chars: 2,
            },
        )
        .unwrap();
        assert!(chunks.iter().any(|chunk| chunk.kind == KgInputKind::Source));
        assert!(
            chunks
                .iter()
                .any(|chunk| chunk.kind == KgInputKind::Session)
        );
        assert!(chunks.iter().any(|chunk| chunk.kind == KgInputKind::Memory));
        assert!(chunks.len() > docs.len());
    }

    #[test]
    fn validates_relation_endpoints() {
        let err = parse_and_validate_extraction(
            r#"{"entities":[{"id":"a","label":"A","kind":"concept"}],"relations":[{"from":"a","to":"missing","kind":"mentions"}]}"#,
        )
        .unwrap_err();
        assert!(err.to_string().contains("unknown to entity"));
    }

    #[test]
    fn hash_extractor_materializes_projection_rows() {
        let docs = vec![KgInputDocument::source(
            "src/lib.rs",
            "Graph projection stores semantic entity rows",
        )];
        let report = extract_documents_to_projection(
            &docs,
            &HashKgExtractor,
            ChunkingConfig {
                max_chars: 120,
                overlap_chars: 0,
            },
        )
        .unwrap();
        assert_eq!(report.chunks.len(), 1);
        assert!(
            report
                .projection
                .nodes
                .iter()
                .any(|node| node.kind == "semantic_entity"
                    && node.properties.get("kg_provider")
                        == Some(&HASH_KG_EXTRACTOR_ID.to_string()))
        );
        assert!(
            report
                .projection
                .edges
                .iter()
                .any(|edge| edge.kind == "semantic_relation")
        );
        assert_eq!(report.backend_candidates[0].backend, "sqlite");
    }

    #[test]
    fn fixture_extractor_records_provider_metadata_and_upserts_sqlite() {
        let docs = vec![KgInputDocument::session(
            "tasks/software/tsift.md",
            "Use GraphProjection rows for KG.",
        )];
        let report = extract_documents_to_projection(
            &docs,
            &FixtureExtractor,
            ChunkingConfig {
                max_chars: 120,
                overlap_chars: 0,
            },
        )
        .unwrap();
        let entity = report
            .projection
            .nodes
            .iter()
            .find(|node| node.kind == "semantic_entity")
            .unwrap();
        assert_eq!(
            entity.properties.get("kg_provider"),
            Some(&"fixture-provider".to_string())
        );
        assert_eq!(
            entity.properties.get("kg_provider_kind"),
            Some(&"llama.cpp".to_string())
        );

        let dir = TempDir::new().unwrap();
        let graph_db = dir.path().join(".tsift/graph.db");
        let upsert = upsert_kg_projection_sqlite(&graph_db, &report.projection).unwrap();
        assert_eq!(upsert.nodes_upserted, report.projection.nodes.len());
        let conn = Connection::open(graph_db).unwrap();
        let semantic_entities: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM graph_nodes WHERE kind = 'semantic_entity'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(semantic_entities, 2);
    }

    /// #kgconf: an extractor that emits a model confidence must have it persisted
    /// (`confidence_source=model`); an extractor that omits confidence must still
    /// yield a node carrying the derived default so gating always has data.
    #[test]
    fn projection_always_persists_confidence_with_source_tag() {
        #[derive(Debug)]
        struct MixedConfidenceExtractor;
        impl KgExtractor for MixedConfidenceExtractor {
            fn metadata(&self) -> KgExtractorMetadata {
                KgExtractorMetadata {
                    provider_id: "mixed-provider".to_string(),
                    provider_kind: ProviderKind::LlamaCpp,
                    extraction_model: "mixed-model".to_string(),
                }
            }
            fn extract_json(&self, _chunk: &KgChunk) -> Result<String> {
                // entity `a` carries a model confidence; entity `b` omits it.
                Ok(serde_json::json!({
                    "entities": [
                        {"id": "a", "label": "WithConf", "kind": "concept", "confidence": 0.42},
                        {"id": "b", "label": "NoConf", "kind": "concept"}
                    ],
                    "relations": [
                        {"from": "a", "to": "b", "kind": "related_to"}
                    ]
                })
                .to_string())
            }
        }

        let docs = vec![KgInputDocument::source(
            "src/lib.rs",
            "WithConf relates to NoConf in the graph.",
        )];
        let report = extract_documents_to_projection(
            &docs,
            &MixedConfidenceExtractor,
            ChunkingConfig {
                max_chars: 120,
                overlap_chars: 0,
            },
        )
        .unwrap();

        let entities: Vec<_> = report
            .projection
            .nodes
            .iter()
            .filter(|node| node.kind == "semantic_entity")
            .collect();
        assert_eq!(entities.len(), 2);
        // Every entity carries a parseable confidence + a source tag.
        for entity in &entities {
            let confidence = entity
                .properties
                .get("confidence")
                .and_then(|c| c.parse::<f64>().ok())
                .expect("every semantic_entity must persist a confidence");
            assert!((0.0..=1.0).contains(&confidence));
            assert!(entity.properties.contains_key("confidence_source"));
        }

        let with_conf = entities
            .iter()
            .find(|e| e.label == "WithConf")
            .unwrap();
        assert_eq!(
            with_conf.properties.get("confidence"),
            Some(&"0.420".to_string())
        );
        assert_eq!(
            with_conf.properties.get("confidence_source"),
            Some(&"model".to_string())
        );

        let no_conf = entities.iter().find(|e| e.label == "NoConf").unwrap();
        assert_eq!(
            no_conf.properties.get("confidence"),
            Some(&format!("{DEFAULT_KG_CONFIDENCE:.3}"))
        );
        assert_eq!(
            no_conf.properties.get("confidence_source"),
            Some(&"default".to_string())
        );

        // The relation edge omits model confidence, so it gets the derived default.
        let relation = report
            .projection
            .edges
            .iter()
            .find(|edge| edge.kind == "semantic_relation")
            .unwrap();
        assert_eq!(
            relation.properties.get("confidence"),
            Some(&format!("{DEFAULT_KG_CONFIDENCE:.3}"))
        );
        assert_eq!(
            relation.properties.get("confidence_source"),
            Some(&"default".to_string())
        );
    }

    /// #kgrefreshdup: re-extracting an edited source (which shifts chunk + node
    /// ids) must REPLACE that source's prior subgraph, not accumulate duplicates,
    /// and must leave other sources untouched.
    #[test]
    fn replace_source_projection_does_not_accumulate_duplicate_entities() {
        let count_for = |conn: &Connection, src: &str| -> i64 {
            conn.query_row(
                "SELECT COUNT(*) FROM graph_nodes n \
                 JOIN graph_node_properties p ON p.node_id = n.id \
                 WHERE n.kind = 'semantic_entity' \
                   AND p.key = 'source_ref' AND p.value = ?1",
                [src],
                |row| row.get(0),
            )
            .unwrap()
        };
        let extract = |source_ref: &str, text: &str| {
            extract_documents_to_projection(
                &[KgInputDocument::source(source_ref, text)],
                &FixtureExtractor,
                ChunkingConfig {
                    max_chars: 500,
                    overlap_chars: 0,
                },
            )
            .unwrap()
            .projection
        };

        let dir = TempDir::new().unwrap();
        let graph_db = dir.path().join(".tsift/graph.db");

        // Initial extraction of two distinct sources.
        replace_kg_source_projection_sqlite(&graph_db, &extract("src/a.rs", "alpha source one"))
            .unwrap();
        replace_kg_source_projection_sqlite(&graph_db, &extract("src/b.rs", "bravo source two"))
            .unwrap();
        {
            let conn = Connection::open(&graph_db).unwrap();
            assert_eq!(count_for(&conn, "src/a.rs"), 2);
            assert_eq!(count_for(&conn, "src/b.rs"), 2);
        }

        // Re-extract source A with EDITED text -> different chunk + entity node
        // ids. A purely-additive upsert would now leave 4 entities for src/a.rs.
        replace_kg_source_projection_sqlite(
            &graph_db,
            &extract("src/a.rs", "alpha source one EDITED with more words"),
        )
        .unwrap();

        let conn = Connection::open(&graph_db).unwrap();
        // Source A replaced (still 2, not 4); source B untouched; total 4, not 6.
        assert_eq!(count_for(&conn, "src/a.rs"), 2);
        assert_eq!(count_for(&conn, "src/b.rs"), 2);
        let total: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM graph_nodes WHERE kind = 'semantic_entity'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(total, 4);
    }

    /// #kgsameas: link_canonical_entities_sqlite writes a same_as edge between
    /// two nodes reconciled to the same canonical entity_id.
    #[test]
    fn link_canonical_entities_writes_same_as_edges() {
        use tsift_core::GraphNode;
        let dir = TempDir::new().unwrap();
        let graph_db = dir.path().join(".tsift/graph.db");
        let projection = GraphProjection {
            nodes: vec![
                GraphNode::new("kgent-1", "semantic_entity", "Dup")
                    .with_property("provider", "tsift-kg")
                    .with_property("entity_id", "kgent-canon"),
                GraphNode::new("kgent-2", "semantic_entity", "Dup")
                    .with_property("provider", "tsift-kg")
                    .with_property("entity_id", "kgent-canon"),
            ],
            edges: vec![],
        };
        upsert_kg_projection_sqlite(&graph_db, &projection).unwrap();

        let written = link_canonical_entities_sqlite(&graph_db).unwrap();
        assert_eq!(written, 1);
        let conn = Connection::open(&graph_db).unwrap();
        let same_as: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM graph_edges WHERE kind = 'same_as'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(same_as, 1);
    }

    #[test]
    fn two_sequential_runs_keep_stable_fact_ids_and_do_not_duplicate_sqlite_rows() {
        let docs = vec![KgInputDocument::source(
            "src/lib.rs",
            "GraphProjection materializes SQLite semantic rows.",
        )];
        let config = ChunkingConfig {
            max_chars: 120,
            overlap_chars: 0,
        };
        let first = extract_documents_to_projection(&docs, &FixtureExtractor, config).unwrap();
        let second = extract_documents_to_projection(&docs, &FixtureExtractor, config).unwrap();
        let verification =
            verify_projection_multi_run_stability(&first.projection, &second.projection).unwrap();

        assert_eq!(
            verification.first_node_count,
            verification.second_node_count
        );
        assert_eq!(
            verification.first_edge_count,
            verification.second_edge_count
        );
        assert_eq!(verification.duplicate_node_ids, Vec::<String>::new());
        assert_eq!(verification.duplicate_edge_ids, Vec::<String>::new());

        let dir = TempDir::new().unwrap();
        let graph_db = dir.path().join(".tsift/graph.db");
        upsert_kg_projection_sqlite(&graph_db, &first.projection).unwrap();
        upsert_kg_projection_sqlite(&graph_db, &second.projection).unwrap();

        let conn = Connection::open(graph_db).unwrap();
        let semantic_entities: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM graph_nodes WHERE kind = 'semantic_entity'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        let semantic_relations: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM graph_edges WHERE kind = 'semantic_relation'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(semantic_entities, 2);
        assert_eq!(semantic_relations, 1);
    }

    /// Records the context pack each chunk was extracted with, so the test can
    /// assert that `extract_documents_to_projection_with_context` wired the
    /// known-entity pack through to the extractor (#kgctxinject).
    #[derive(Debug, Default)]
    struct ContextRecordingExtractor {
        seen_entity_ids: std::cell::RefCell<Vec<String>>,
    }

    impl KgExtractor for ContextRecordingExtractor {
        fn metadata(&self) -> KgExtractorMetadata {
            KgExtractorMetadata {
                provider_id: "ctx-recording".to_string(),
                provider_kind: ProviderKind::LlamaCpp,
                extraction_model: "ctx-model".to_string(),
            }
        }

        fn extract_json(&self, _chunk: &KgChunk) -> Result<String> {
            Ok(r#"{"entities":[],"relations":[]}"#.to_string())
        }

        fn extract_json_with_context(
            &self,
            chunk: &KgChunk,
            context: Option<&context_pack::ContextPack>,
        ) -> Result<String> {
            if let Some(pack) = context {
                self.seen_entity_ids
                    .borrow_mut()
                    .extend(pack.entities.iter().map(|e| e.node_id.clone()));
            }
            self.extract_json(chunk)
        }
    }

    #[test]
    fn none_context_source_behaves_like_plain_extract() {
        // #kgctxinject: passing `None` must not invoke the context path.
        let docs = vec![KgInputDocument::source("src/lib.rs", "GraphProjection rows")];
        let extractor = ContextRecordingExtractor::default();
        extract_documents_to_projection_with_context(
            &docs,
            &extractor,
            ChunkingConfig::default(),
            None,
        )
        .unwrap();
        assert!(extractor.seen_entity_ids.borrow().is_empty());
    }

    #[test]
    fn context_source_threads_known_entity_pack_into_extractor() {
        // The chunk mentions "GraphProjection"; the existing graph holds a
        // matching `semantic_entity`, so its canonical stable id must be handed
        // to the extractor for reconciliation.
        let mut existing = GraphProjection::default();
        existing.nodes.push(
            tsift_core::GraphNode::new("kgent-canonical", "semantic_entity", "GraphProjection")
                .with_property("entity_kind", "type")
                .with_property("confidence", "0.900"),
        );
        let source = context_pack::ChunkContextSource::new(
            &existing,
            context_pack::ContextPackConfig::default(),
        );

        let docs = vec![KgInputDocument::source(
            "src/lib.rs",
            "This document discusses the GraphProjection type at length.",
        )];
        let extractor = ContextRecordingExtractor::default();
        extract_documents_to_projection_with_context(
            &docs,
            &extractor,
            ChunkingConfig::default(),
            Some(&source),
        )
        .unwrap();
        assert!(
            extractor
                .seen_entity_ids
                .borrow()
                .contains(&"kgent-canonical".to_string()),
            "extractor should receive the canonical stable id for reconciliation"
        );
    }
}
