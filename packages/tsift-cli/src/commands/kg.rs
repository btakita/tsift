//! `tsift kg` — local Knowledge Graph extraction via a lazy local model (#lmlazy).
//!
//! The `extract` subcommand reads source text, resolves an Ollama-served model
//! (by explicit `--model` tag or by `--profile`), runs it through the shared
//! `tsift-kg` pipeline, and either prints a human summary or upserts the
//! resulting `GraphProjection` into a `.tsift/graph.db`.
use anyhow::{Context, Result, bail};
use std::io::Read;
use std::path::{Path, PathBuf};

use tsift_kg::{
    ChunkingConfig, KgInputDocument, KgInputKind, OllamaKgExtractor, extract_documents_to_projection,
    upsert_kg_projection_sqlite,
};
use tsift_local_model::profile_by_id;

/// Default extractor profile when neither `--model` nor `--profile` is given.
/// Picked because it is the smallest GPU-resident extractor served by Ollama in
/// the default profile set.
const DEFAULT_PROFILE_ID: &str = "qwen3-32b-q4-ollama";

pub(crate) fn cmd_kg_extract(
    profile: Option<String>,
    model: Option<String>,
    host: Option<String>,
    input: Option<PathBuf>,
    source_ref: Option<String>,
    graph_db: Option<PathBuf>,
    json: bool,
) -> Result<()> {
    let (text, default_source_ref) = read_input(input.as_deref())?;
    if text.trim().is_empty() {
        bail!("no source text to extract — provide --input <file> or pipe text on stdin");
    }
    let source_ref = source_ref.unwrap_or(default_source_ref);

    let resolved_model = resolve_model_tag(profile.as_deref(), model.as_deref())?;
    let extractor = match host.as_deref() {
        Some(host) => OllamaKgExtractor::new(&resolved_model).with_host(host),
        None => OllamaKgExtractor::new(&resolved_model),
    };

    let document = KgInputDocument::new(KgInputKind::Source, &source_ref, &text);
    let report = extract_documents_to_projection(&[document], &extractor, ChunkingConfig::default())
        .context("KG extraction pipeline failed")?;

    let entity_count: usize = report.projection.nodes.len();
    let relation_count: usize = report.projection.edges.len();

    let upsert = if let Some(graph_db) = graph_db.as_deref() {
        Some(upsert_kg_projection_sqlite(graph_db, &report.projection).context(
            format!("upserting KG projection into {}", graph_db.display()),
        )?)
    } else {
        None
    };

    if json {
        let payload = serde_json::json!({
            "provider_id": report.extracted_chunks.first().map(|c| c.chunk.id.as_str()).unwrap_or(""),
            "model": extractor.model(),
            "host": extractor.host(),
            "source_ref": source_ref,
            "chunks": report.chunks.len(),
            "entities": entity_count,
            "relations": relation_count,
            "upsert": upsert,
        });
        println!("{}", serde_json::to_string_pretty(&payload)?);
    } else {
        println!("KG extraction");
        println!("Model: {} ({})", extractor.model(), extractor.host());
        println!(
            "Chunks: {}  Entities: {}  Relations: {}",
            report.chunks.len(),
            entity_count,
            relation_count
        );
        for node in report.projection.nodes.iter().take(20) {
            println!("  - {} [{}]", node.label, node.kind);
        }
        if report.projection.nodes.len() > 20 {
            println!("  ... ({} more)", report.projection.nodes.len() - 20);
        }
        if let Some(graph_db) = graph_db.as_deref() {
            println!("Upserted into: {}", graph_db.display());
        }
    }
    Ok(())
}

fn read_input(input: Option<&Path>) -> Result<(String, String)> {
    match input {
        Some(path) => {
            let text = std::fs::read_to_string(path)
                .with_context(|| format!("reading input file {}", path.display()))?;
            Ok((text, path.display().to_string()))
        }
        None => {
            let mut text = String::new();
            std::io::stdin()
                .read_to_string(&mut text)
                .context("reading source text from stdin")?;
            Ok((text, "stdin".to_string()))
        }
    }
}

/// Resolve the Ollama model tag. `--model` wins; otherwise look up the profile's
/// `model_ref`; otherwise default to `DEFAULT_PROFILE_ID`'s tag.
fn resolve_model_tag(profile: Option<&str>, model: Option<&str>) -> Result<String> {
    if let Some(model) = model {
        return Ok(model.to_string());
    }
    let profile_id = profile.unwrap_or(DEFAULT_PROFILE_ID);
    let profile = profile_by_id(profile_id).ok_or_else(|| {
        anyhow::anyhow!(
            "unknown tsift local-model profile `{profile_id}`. \
             Use --model <ollama-tag> or one of: {}",
            list_profile_ids().join(", ")
        )
    })?;
    Ok(profile.model_ref.to_string())
}

fn list_profile_ids() -> Vec<String> {
    tsift_local_model::default_model_profiles()
        .into_iter()
        .map(|p| p.id.to_string())
        .collect()
}
