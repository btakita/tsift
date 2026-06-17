use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;
use tsift_core::{GraphEdge, GraphFreshness, GraphNode, GraphProjection, GraphProvenance};
use tsift_local_model::ProviderKind;
use tsift_sqlite::SqliteGraphStore;

pub const KG_CONTRACT_VERSION: &str = "tsift-kg-v1";
pub const HASH_KG_EXTRACTOR_ID: &str = "tsift-local-hash-v1";

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
    let chunks = chunk_documents(documents, config)?;
    let mut extracted_chunks = Vec::new();
    for chunk in &chunks {
        let raw_json = extractor
            .extract_json(chunk)
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
    if let Some(confidence) = entity.confidence {
        node = node.with_property("confidence", format!("{confidence:.3}"));
    }
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
    if let Some(confidence) = relation.confidence {
        edge = edge.with_property("confidence", format!("{confidence:.3}"));
    }
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
}
