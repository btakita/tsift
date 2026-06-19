//! Ollama-backed KG extractor — the lazy local-model inference client.
//!
//! Calls an Ollama `/api/chat` endpoint with a structured-output JSON schema and
//! feeds the model's JSON through the shared `KgExtractionPayload` validator.
//! The base URL defaults to `http://127.0.0.1:11434` and honors ollama's own
//! `OLLAMA_HOST` env var; the model tag is the ollama pull name (for example
//! `hf.co/Qwen/Qwen3-32B-GGUF:Q4_K_M`).

use crate::context_pack::{ContextEntity, ContextPack};
use crate::{KgChunk, KgExtractionPayload, KgExtractor, KgExtractorMetadata};
use anyhow::{Context, Result, bail};
use serde::Deserialize;
use serde_json::{Value, json};
use std::collections::HashSet;
use std::time::Duration;
use tsift_local_model::ProviderKind;

pub const DEFAULT_OLLAMA_HOST: &str = "http://127.0.0.1:11434";
pub const OLLAMA_PROVIDER_ID: &str = "tsift-ollama";
const DEFAULT_TIMEOUT_SECONDS: u64 = 600;

const SYSTEM_PROMPT: &str = "\
You are a knowledge-graph extraction engine. Read the provided source text and \
extract entities and the relations between them as strict JSON.\n\n\
Output schema:\n\
{\n  \"entities\": [\n    {\"id\": \"e0\", \"label\": string, \"kind\": string, \"description\": string, \"confidence\": number}\n  ],\n  \"relations\": [\n    {\"from\": \"e0\", \"to\": \"e1\", \"kind\": string, \"label\": string, \"confidence\": number}\n  ]\n}\n\n\
Rules:\n\
- \"id\" is a short stable slug unique within this chunk (e0, e1, e2, ...).\n\
- \"from\" and \"to\" MUST reference an entity \"id\" present in this same output.\n\
- \"kind\" is a lowercase noun describing the entity or relation type (concept, function, module, type, calls, depends_on, related_to, ...).\n\
- \"confidence\" is a float in 0.0..=1.0.\n\
- Omit entities and relations you cannot ground in the text. Prefer precision over recall.\n\
- Return ONLY the JSON object. No prose, no markdown code fences.";

/// Ollama-backed KG extractor. Cheap to clone; owns its endpoint + model tag.
#[derive(Debug, Clone)]
pub struct OllamaKgExtractor {
    host: String,
    model: String,
    timeout: Duration,
}

impl OllamaKgExtractor {
    pub fn new(model: impl Into<String>) -> Self {
        Self {
            host: resolve_ollama_host(),
            model: model.into(),
            timeout: Duration::from_secs(DEFAULT_TIMEOUT_SECONDS),
        }
    }

    /// Override the Ollama base URL (e.g. `http://127.0.0.1:11434`). Blank falls
    /// back to the resolved default so an empty override cannot break a run.
    pub fn with_host(mut self, host: impl Into<String>) -> Self {
        let host = host.into();
        if !host.trim().is_empty() {
            self.host = host;
        }
        self
    }

    /// Override the per-request timeout (default 600s — large models load slowly).
    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    pub fn host(&self) -> &str {
        &self.host
    }

    pub fn model(&self) -> &str {
        &self.model
    }

    /// Build the `/api/chat` request body for a chunk. Separated from the HTTP
    /// call so tests can assert the prompt + schema without a live model.
    pub fn chat_request_body(&self, chunk_text: &str) -> Value {
        self.chat_request_body_with_context(chunk_text, None)
    }

    /// Context-aware prompt builder (`#kgctxinject`). Identical to
    /// [`chat_request_body`](Self::chat_request_body) when `context` is `None`
    /// or empty, so the no-context path stays byte-compatible. When a non-empty
    /// known-entity pack is supplied, a bounded `[KNOWN ENTITIES]` block is
    /// prepended to the user message directing the model to reuse those exact
    /// stable ids when the chunk references a matching entity (capped by the
    /// pack's `max_entities`, which bounds the prompt-size / context-window
    /// cost — the same discipline as the bounded retrieval scan).
    pub fn chat_request_body_with_context(
        &self,
        chunk_text: &str,
        context: Option<&ContextPack>,
    ) -> Value {
        let user_content = match context {
            Some(pack) if !pack.entities.is_empty() => format!(
                "{}\n\n[CHUNK]\n{}",
                format_known_entities(&pack.entities),
                chunk_text
            ),
            _ => chunk_text.to_string(),
        };
        json!({
            "model": self.model,
            "stream": false,
            "format": payload_schema(),
            "options": { "temperature": 0 },
            "messages": [
                { "role": "system", "content": SYSTEM_PROMPT },
                { "role": "user", "content": user_content },
            ],
        })
    }

    fn chat_url(&self) -> String {
        format!("{}/api/chat", self.host.trim_end_matches('/'))
    }

    /// Send a built `/api/chat` request body and return the repaired assistant
    /// extraction JSON. Shared by the plain and context-aware extract paths.
    fn send_chat_request(&self, body: &Value) -> Result<String> {
        let url = self.chat_url();
        let agent = ureq::Agent::config_builder()
            .http_status_as_error(false)
            .timeout_global(Some(self.timeout))
            .build()
            .new_agent();
        let mut response = agent
            .post(&url)
            .send_json(body)
            .with_context(|| format!("calling Ollama chat endpoint {url}"))?;
        let status = response.status();
        let content = response
            .body_mut()
            .read_to_string()
            .with_context(|| format!("reading Ollama chat response body (HTTP {status})"))?;
        if !status.is_success() {
            bail!(
                "Ollama chat request failed (HTTP {status}): {}",
                truncate_str(&content, 500)
            );
        }
        let parsed: OllamaChatResponse = serde_json::from_str(&content)
            .with_context(|| format!("parsing Ollama chat response JSON (HTTP {status})"))?;
        let assistant = parsed.message.content.as_deref().unwrap_or("");
        repair_extraction_payload(assistant)
    }
}

/// Render the bounded known-entity pack as a compact instruction block. Each
/// line is `stable_id | label | kind | confidence`. Deterministic ordering
/// follows the pack's already-ranked entity list.
fn format_known_entities(entities: &[ContextEntity]) -> String {
    let mut lines = vec![
        "[KNOWN ENTITIES — reuse these exact stable ids when the chunk references a matching entity; \
         do not invent new ids for them. Omit a listed entity only if it does not actually appear in the text.]"
            .to_string(),
    ];
    for e in entities {
        lines.push(format!(
            "- {} | {} | {} | {:.3}",
            e.node_id, e.label, e.kind, e.confidence
        ));
    }
    lines.join("\n")
}

impl KgExtractor for OllamaKgExtractor {
    fn metadata(&self) -> KgExtractorMetadata {
        KgExtractorMetadata {
            provider_id: OLLAMA_PROVIDER_ID.to_string(),
            provider_kind: ProviderKind::Ollama,
            extraction_model: self.model.clone(),
        }
    }

    fn extract_json(&self, chunk: &KgChunk) -> Result<String> {
        let body = self.chat_request_body(&chunk.text);
        self.send_chat_request(&body)
    }

    /// `#kgctxinject`: inject the known-entity pack into the prompt so the model
    /// reconciles against canonical stable ids. Falls back to the plain path
    /// when no pack is supplied.
    fn extract_json_with_context(
        &self,
        chunk: &KgChunk,
        context: Option<&ContextPack>,
    ) -> Result<String> {
        let body = self.chat_request_body_with_context(&chunk.text, context);
        self.send_chat_request(&body)
    }
}

#[derive(Debug, Deserialize)]
struct OllamaChatResponse {
    #[serde(default)]
    message: OllamaChatMessage,
}

#[derive(Debug, Default, Deserialize)]
struct OllamaChatMessage {
    #[serde(default)]
    content: Option<String>,
}

/// Resolve the Ollama base URL. `OLLAMA_HOST` wins when set + non-empty (ollama's
/// own convention), otherwise the compile-time default.
fn resolve_ollama_host() -> String {
    match std::env::var("OLLAMA_HOST") {
        Ok(host) if !host.trim().is_empty() => {
            // OLLAMA_HOST may be host:port without a scheme.
            if host.starts_with("http://") || host.starts_with("https://") {
                host
            } else {
                format!("http://{host}")
            }
        }
        _ => DEFAULT_OLLAMA_HOST.to_string(),
    }
}

/// Structured-output JSON schema sent as Ollama `format` to constrain decoding.
/// Mirrors `KgExtractionPayload` so model output validates without coercion.
fn payload_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "entities": {
                "type": "array",
                "items": {
                    "type": "object",
                    "properties": {
                        "id": { "type": "string" },
                        "label": { "type": "string" },
                        "kind": { "type": "string" },
                        "description": { "type": "string" },
                        "confidence": { "type": "number" }
                    },
                    // #kgconf: require `confidence` so the structured decoder emits a
                    // real per-entity score instead of dropping the optional field
                    // (which left every projected node confidence-less).
                    "required": ["id", "label", "kind", "confidence"]
                }
            },
            "relations": {
                "type": "array",
                "items": {
                    "type": "object",
                    "properties": {
                        "from": { "type": "string" },
                        "to": { "type": "string" },
                        "kind": { "type": "string" },
                        "label": { "type": "string" },
                        "confidence": { "type": "number" }
                    },
                    // #kgconf: require `confidence` on relations for the same reason.
                    "required": ["from", "to", "kind", "confidence"]
                }
            }
        },
        "required": ["entities", "relations"]
    })
}

/// Extract the outermost JSON object from a model response. Handles clean JSON,
/// markdown ``` fences, and prose-wrapped output as a defensive fallback (the
/// structured `format` should make this a no-op, but models occasionally stray).
pub fn extract_json_object(s: &str) -> &str {
    let trimmed = s.trim();
    let after_fence = trimmed
        .strip_prefix("```json")
        .or_else(|| trimmed.strip_prefix("```"))
        .map(str::trim_start)
        .unwrap_or(trimmed)
        .strip_suffix("```")
        .unwrap_or(trimmed)
        .trim();
    if let (Some(start), Some(end)) = (after_fence.find('{'), after_fence.rfind('}'))
        && start <= end
    {
        return &after_fence[start..=end];
    }
    after_fence
}

/// Repair imperfect model output so it passes the shared strict validator:
/// drop empty/duplicate entity ids, drop relations that reference unknown
/// endpoints, and clamp confidence to 0.0..=1.0. Returns the repaired payload
/// re-serialized as JSON. The structured `format` schema cannot enforce
/// cross-references (`from`/`to` -> entity `id`), so this is the safety net.
pub fn repair_extraction_payload(raw: &str) -> Result<String> {
    let json_str = extract_json_object(raw);
    let mut payload: KgExtractionPayload =
        serde_json::from_str(json_str).context("parsing Ollama extraction JSON")?;

    // Deduplicate entity ids and drop entities missing required fields (keep first).
    let mut seen = HashSet::new();
    payload.entities.retain(|e| {
        !e.id.is_empty()
            && !e.label.is_empty()
            && !e.kind.is_empty()
            && seen.insert(e.id.clone())
    });

    // Drop relations that reference unknown/empty endpoints or miss a kind.
    let valid_ids: HashSet<&str> = payload.entities.iter().map(|e| e.id.as_str()).collect();
    payload.relations.retain(|r| {
        !r.from.is_empty()
            && !r.to.is_empty()
            && !r.kind.is_empty()
            && valid_ids.contains(r.from.as_str())
            && valid_ids.contains(r.to.as_str())
    });

    // Clamp confidence (models occasionally emit values > 1.0).
    for entity in &mut payload.entities {
        if let Some(confidence) = entity.confidence {
            entity.confidence = Some(confidence.clamp(0.0, 1.0));
        }
    }
    for relation in &mut payload.relations {
        if let Some(confidence) = relation.confidence {
            relation.confidence = Some(confidence.clamp(0.0, 1.0));
        }
    }

    serde_json::to_string(&payload).map_err(Into::into)
}

fn truncate_str(s: &str, max: usize) -> String {
    if s.len() <= max {
        s.to_string()
    } else {
        format!("{}…", &s[..max])
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{ChunkingConfig, KgInputDocument, KgInputKind, extract_documents_to_projection};

    #[test]
    fn extract_json_object_handles_clean_json() {
        assert_eq!(
            extract_json_object(r#"{"entities":[],"relations":[]}"#),
            r#"{"entities":[],"relations":[]}"#
        );
    }

    #[test]
    fn extract_json_object_strips_markdown_fences() {
        let raw = "```json\n{\"entities\":[],\"relations\":[]}\n```";
        assert_eq!(
            extract_json_object(raw),
            r#"{"entities":[],"relations":[]}"#
        );
    }

    #[test]
    fn extract_json_object_recovers_from_prose_wrap() {
        let raw = "Here is the extraction:\n{\"entities\":[{\"id\":\"e0\"}],\"relations\":[]}\nDone.";
        assert_eq!(
            extract_json_object(raw),
            "{\"entities\":[{\"id\":\"e0\"}],\"relations\":[]}"
        );
    }

    #[test]
    fn request_body_carries_model_schema_and_zero_temperature() {
        let extractor = OllamaKgExtractor::new("hf.co/Qwen/Qwen3-32B-GGUF:Q4_K_M");
        let body = extractor.chat_request_body("sample text");
        assert_eq!(body["model"], "hf.co/Qwen/Qwen3-32B-GGUF:Q4_K_M");
        assert_eq!(body["stream"], false);
        assert_eq!(body["options"]["temperature"], 0);
        assert_eq!(body["messages"][0]["role"], "system");
        assert_eq!(body["messages"][1]["role"], "user");
        assert_eq!(body["messages"][1]["content"], "sample text");
        // Structured output schema constrains entities/relations.
        assert_eq!(body["format"]["type"], "object");
        assert_eq!(body["format"]["required"][0], "entities");
        // #kgconf: confidence is a required field so the decoder emits a real
        // per-entity/relation score instead of dropping the optional property.
        let entity_required = &body["format"]["properties"]["entities"]["items"]["required"];
        assert!(
            entity_required
                .as_array()
                .unwrap()
                .iter()
                .any(|v| v == "confidence"),
            "entity schema must require confidence"
        );
        let relation_required = &body["format"]["properties"]["relations"]["items"]["required"];
        assert!(
            relation_required
                .as_array()
                .unwrap()
                .iter()
                .any(|v| v == "confidence"),
            "relation schema must require confidence"
        );
    }

    #[test]
    fn metadata_reports_ollama_provider_and_model_tag() {
        let extractor = OllamaKgExtractor::new("hf.co/Qwen/Qwen3-32B-GGUF:Q4_K_M");
        let metadata = extractor.metadata();
        assert_eq!(metadata.provider_id, OLLAMA_PROVIDER_ID);
        assert_eq!(metadata.provider_kind, ProviderKind::Ollama);
        assert_eq!(metadata.extraction_model, "hf.co/Qwen/Qwen3-32B-GGUF:Q4_K_M");
    }

    #[test]
    fn with_host_blank_falls_back_to_default() {
        let extractor = OllamaKgExtractor::new("m").with_host("   ");
        assert_eq!(extractor.host(), DEFAULT_OLLAMA_HOST);
    }

    /// A stub extractor that returns a canned model response, to exercise the
    /// parsing/validation pipeline the way a real Ollama call would.
    struct StubOllamaLike;
    impl KgExtractor for StubOllamaLike {
        fn metadata(&self) -> KgExtractorMetadata {
            KgExtractorMetadata {
                provider_id: OLLAMA_PROVIDER_ID.to_string(),
                provider_kind: ProviderKind::Ollama,
                extraction_model: "stub-model".to_string(),
            }
        }
        fn extract_json(&self, _chunk: &KgChunk) -> Result<String> {
            // A real OllamaKgExtractor cleans a fenced/prose-wrapped response down
            // to the JSON object before returning (see extract_json_object). The
            // pipeline parses extract_json output directly, so the stub returns
            // the already-cleaned payload.
            Ok(
                r#"{"entities":[{"id":"e0","label":"tsift","kind":"concept","confidence":0.9}],"relations":[]}"#
                    .to_string(),
            )
        }
    }

    #[test]
    fn ollama_style_response_feeds_pipeline_through_validator() {
        // The stub returns prose+fenced JSON; the pipeline must parse+validate it
        // exactly like an Ollama response that slipped past structured output.
        let docs = vec![KgInputDocument::new(
            KgInputKind::Source,
            "test.md",
            "tsift local model smoke",
        )];
        let report =
            extract_documents_to_projection(&docs, &StubOllamaLike, ChunkingConfig::default())
                .expect("pipeline accepts ollama-style JSON");
        assert_eq!(report.extracted_chunks.len(), 1);
        let payload = &report.extracted_chunks[0].payload;
        assert_eq!(payload.entities.len(), 1);
        assert_eq!(payload.entities[0].label, "tsift");
    }

    #[test]
    fn cleaning_a_fenced_response_yields_schema_valid_json() {
        use crate::parse_and_validate_extraction;
        // Simulate a model that ignored structured output and wrapped its JSON.
        let raw = "Here you go:\n```json\n{\"entities\":[{\"id\":\"e0\",\"label\":\"lease\",\"kind\":\"concept\",\"confidence\":0.8}],\"relations\":[]}\n```\n";
        let cleaned = extract_json_object(raw);
        let payload =
            parse_and_validate_extraction(cleaned).expect("cleaned JSON validates against schema");
        assert_eq!(payload.entities.len(), 1);
        assert_eq!(payload.entities[0].label, "lease");
    }

    #[test]
    fn repair_drops_dangling_relations_and_clamps_confidence() {
        use crate::parse_and_validate_extraction;
        // Mirrors the live 32B failure: a relation references a non-existent
        // entity id, and one confidence is out of range. Repair must drop the
        // dangling relation and clamp the confidence so the strict validator
        // accepts the payload.
        let raw = r#"{
            "entities": [
                {"id": "e0", "label": "tsift", "kind": "concept", "confidence": 1.5},
                {"id": "e1", "label": "lease", "kind": "concept"}
            ],
            "relations": [
                {"from": "e0", "to": "e1", "kind": "depends_on", "confidence": 0.9},
                {"from": "e0", "to": "e8", "kind": "calls"}
            ]
        }"#;
        let repaired = repair_extraction_payload(raw).expect("repair serializes JSON");
        let payload =
            parse_and_validate_extraction(&repaired).expect("repaired payload validates strictly");
        assert_eq!(payload.entities.len(), 2);
        assert_eq!(payload.entities[0].confidence, Some(1.0)); // clamped from 1.5
        assert_eq!(payload.relations.len(), 1); // dangling e8 relation dropped
        assert_eq!(payload.relations[0].to, "e1");
    }

    /// Live Ollama smoke — only runs when `TSIFT_OLLAMA_SMOKE=1` and
    /// `TSIFT_OLLAMA_SMOKE_MODEL=<tag>` are set, so `cargo test` stays hermetic.
    #[test]
    fn live_ollama_extract_roundtrip() {
        let Ok(model) = std::env::var("TSIFT_OLLAMA_SMOKE_MODEL") else {
            return;
        };
        if std::env::var("TSIFT_OLLAMA_SMOKE").as_deref() != Ok("1") {
            return;
        }
        let extractor = OllamaKgExtractor::new(model);
        let docs = vec![KgInputDocument::new(
            KgInputKind::Source,
            "smoke.md",
            "The tsift local-model crate owns GPU probing and the cooperative lease registry. \
             tsift-kg consumes the lease registry to serialize extractor runs on a single GPU.",
        )];
        let report =
            extract_documents_to_projection(&docs, &extractor, ChunkingConfig::default())
                .expect("live Ollama extraction should succeed");
        assert!(!report.projection.nodes.is_empty());
    }

    fn known_entity(node_id: &str, label: &str, kind: &str, confidence: f64) -> ContextEntity {
        ContextEntity {
            node_id: node_id.to_string(),
            label: label.to_string(),
            kind: kind.to_string(),
            confidence,
            degree: 0,
            matched_seed: None,
        }
    }

    #[test]
    fn context_aware_body_without_pack_is_byte_identical_to_plain() {
        // #kgctxinject: the no-context path must stay byte-for-byte the plain
        // prompt so an empty/new graph never changes extraction behavior.
        let extractor = OllamaKgExtractor::new("m");
        let plain = extractor.chat_request_body("some chunk text");
        let none_ctx = extractor.chat_request_body_with_context("some chunk text", None);
        assert_eq!(plain, none_ctx);

        let empty_pack = ContextPack {
            entities: vec![],
            truncated: false,
        };
        let empty_ctx =
            extractor.chat_request_body_with_context("some chunk text", Some(&empty_pack));
        assert_eq!(plain, empty_ctx);
    }

    #[test]
    fn context_aware_body_prepends_known_entities_block() {
        // A non-empty pack prepends the bounded [KNOWN ENTITIES] block with the
        // canonical stable id so the model reconciles instead of re-inventing.
        let extractor = OllamaKgExtractor::new("m");
        let pack = ContextPack {
            entities: vec![known_entity("kgent-abc123", "OllamaKgExtractor", "type", 0.9)],
            truncated: false,
        };
        let body = extractor.chat_request_body_with_context("describe OllamaKgExtractor", Some(&pack));
        let content = body["messages"][1]["content"].as_str().unwrap();
        assert!(content.contains("[KNOWN ENTITIES"));
        assert!(content.contains("kgent-abc123"));
        assert!(content.contains("OllamaKgExtractor"));
        // The original chunk text follows the context block under a [CHUNK] marker.
        assert!(content.contains("[CHUNK]\ndescribe OllamaKgExtractor"));
        // System prompt + structured-output schema are unchanged.
        assert_eq!(body["messages"][0]["role"], "system");
        assert_eq!(body["format"]["type"], "object");
    }

    #[test]
    fn format_known_entities_emits_one_line_per_entity() {
        let block = format_known_entities(&[
            known_entity("kgent-a", "Alpha", "concept", 0.80),
            known_entity("kgent-b", "Beta", "type", 0.50),
        ]);
        assert!(block.starts_with("[KNOWN ENTITIES"));
        assert!(block.contains("- kgent-a | Alpha | concept | 0.800"));
        assert!(block.contains("- kgent-b | Beta | type | 0.500"));
    }
}
