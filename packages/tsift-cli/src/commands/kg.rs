//! `tsift kg` — local Knowledge Graph extraction via a lazy local model (#lmlazy).
//!
//! The `extract` subcommand reads source text, resolves an Ollama-served model
//! (by explicit `--model` tag or by `--profile`), runs it through the shared
//! `tsift-kg` pipeline, and either prints a human summary or upserts the
//! resulting `GraphProjection` into a `.tsift/graph.db`.
//!
//! `status` / `unload` / `smoke` complete the operational surface required by
//! `specs/local-kg-model.md` line 31-32 (#kgcliext).
use anyhow::{Context, Result, bail};
use serde::Serialize;
use std::collections::BTreeMap;
use std::io::Read;
use std::path::{Path, PathBuf};

use tsift_core::{GraphProjection, GraphStore};
use tsift_kg::context_pack::{ChunkContextSource, ContextPackConfig};
use tsift_kg::{
    ChunkingConfig, KgInputDocument, KgInputKind, KgSqliteUpsertReport, OllamaKgExtractor,
    extract_documents_to_projection, extract_documents_to_projection_with_context,
    upsert_kg_projection_sqlite,
};
use tsift_local_model::profile_by_id;

/// Default extractor profile when neither `--model` nor `--profile` is given.
/// Picked because it is the smallest GPU-resident extractor served by Ollama in
/// the default profile set.
const DEFAULT_PROFILE_ID: &str = "qwen3-32b-q4-ollama";

/// Default `.tsift/graph.db` location for `tsift kg status` when `--graph-db`
/// is not supplied. Resolves against the current working directory.
const DEFAULT_GRAPH_DB_RELATIVE: &str = ".tsift/graph.db";

/// Sample text used by `tsift kg smoke` to exercise the KG pipeline
/// end-to-end against a live Ollama server.
const SMOKE_SAMPLE_TEXT: &str = "\
The tsift local-model crate owns GPU probing and the cooperative lease registry. \
tsift-kg consumes the lease registry to serialize extractor runs on a single GPU. \
OllamaKgExtractor posts to /api/chat with a structured-output JSON schema.";
const SMOKE_SOURCE_REF: &str = "tsift-kg-smoke";

/// Arguments for `tsift kg extract` (#kgleasewire adds lease coordination).
pub(crate) struct KgExtractArgs {
    pub profile: Option<String>,
    pub model: Option<String>,
    pub host: Option<String>,
    pub input: Option<PathBuf>,
    pub source_ref: Option<String>,
    pub graph_db: Option<PathBuf>,
    pub no_lease: bool,
    pub idle_ttl_seconds: u64,
    pub keep_loaded: bool,
    pub lease_file: Option<PathBuf>,
    /// Skip graph-aware context injection (#kgctxinject). By default, when
    /// `--graph-db` already holds `semantic_entity` nodes, a bounded
    /// known-entity pack is injected into the extractor prompt so the model
    /// reconciles against canonical stable ids instead of re-inventing them.
    pub no_context: bool,
    pub json: bool,
}

/// Arguments for `tsift kg refresh` (#kgrefreshapply adds `--apply`).
///
/// Without `apply`, refresh is the read-only staleness plan from
/// `#kgextractrefresh`. With `apply`, every stale / no_recorded_hash source
/// whose file is still readable is re-extracted through the lease-aware
/// `kg extract` path; the extract pass-through fields configure that
/// re-extraction.
pub(crate) struct KgRefreshArgs {
    pub graph_db: Option<PathBuf>,
    pub json: bool,
    pub apply: bool,
    pub profile: Option<String>,
    pub model: Option<String>,
    pub host: Option<String>,
    pub no_lease: bool,
    pub idle_ttl_seconds: u64,
    pub keep_loaded: bool,
    pub lease_file: Option<PathBuf>,
    /// Skip graph-aware context injection during `--apply` re-extraction
    /// (#kgctxincremental). By default re-extraction reconciles against the
    /// existing graph's stable ids instead of duplicating them.
    pub no_context: bool,
}

/// Resolve the profile id used for cooperative GPU leasing during an extract.
///
/// An explicit `--model` bypasses profile resolution entirely, so there is no
/// profile to lease (returns `None`). Otherwise the chosen profile — or the
/// default — is leased so concurrent extracts serialize on the GPU.
pub(crate) fn resolve_lease_profile_id(
    profile: Option<&str>,
    model: Option<&str>,
) -> Option<String> {
    if model.is_some() {
        return None;
    }
    Some(profile.unwrap_or(DEFAULT_PROFILE_ID).to_string())
}

/// Outcome of one KG extract. Returned by `run_kg_extract` so `kg extract`
/// and `kg refresh --apply` share one lease-aware path and one print format;
/// the `--apply` loop collects a `Vec<KgExtractOutcome>` to emit a unified
/// summary instead of interleaving per-source stdout (#kgrefreshapply).
#[derive(Debug, Serialize)]
pub(crate) struct KgExtractOutcome {
    pub provider_id: String,
    pub model: String,
    pub host: String,
    pub source_ref: String,
    pub chunks: usize,
    pub entities: usize,
    pub relations: usize,
    pub upsert: Option<KgSqliteUpsertReport>,
    /// Up to 20 `(label, kind)` pairs backing the `kg extract` human preview.
    pub node_preview: Vec<(String, String)>,
    pub node_total: usize,
    /// Human-facing unload notice emitted when this extract dropped the last
    /// lease reference (reference-counted unload, #kgleasewire). `None` when the
    /// model was kept loaded or leasing was bypassed.
    pub unloaded: Option<String>,
    /// Count of existing `semantic_entity` nodes offered to the extractor as
    /// graph-aware context (#kgctxinject). `None` when no graph-db context was
    /// injected (no `--graph-db`, an empty/new graph, or `--no-context`).
    pub context_entities: Option<usize>,
}

/// Core extract pipeline with lease coordination (#kgleasewire). Acquires an
/// exclusive cooperative GPU lease, runs the KG pipeline, upserts the
/// projection, then releases the lease (reference-counted unload on the success
/// path). Returns the extraction outcome without printing. A bailed extract
/// leaves a pid-dead holder reclaimed by the next acquire/reap (crash-safe via
/// #kgreflease), so the lease is only released on the success path.
pub(crate) fn run_kg_extract(args: KgExtractArgs) -> Result<KgExtractOutcome> {
    let KgExtractArgs {
        profile,
        model,
        host,
        input,
        source_ref,
        graph_db,
        no_lease,
        idle_ttl_seconds,
        keep_loaded,
        lease_file,
        no_context,
        json: _,
    } = args;
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

    // #kgleasewire: acquire an exclusive cooperative GPU lease before loading
    // the model so concurrent extracts serialize on one GPU.
    let lease_profile = if no_lease {
        None
    } else {
        resolve_lease_profile_id(profile.as_deref(), model.as_deref())
    };
    let lease_path = tsift_local_model::resolve_lease_file(lease_file.as_deref());
    let holder_pid = std::process::id();
    if let Some(ref lease_id) = lease_profile {
        let acquisition = tsift_local_model::acquire_lease(
            lease_id,
            holder_pid,
            "tsift kg extract",
            0,
            idle_ttl_seconds,
            tsift_local_model::current_unix_seconds(),
            &lease_path,
        )
        .with_context(|| format!("acquiring GPU lease for {lease_id}"))?;
        if acquisition.status == tsift_local_model::GpuLeaseAcquisitionStatus::Conflict {
            let held_by = acquisition
                .conflict
                .map(|c| c.holder_pid.to_string())
                .unwrap_or_else(|| "unknown".to_string());
            bail!(
                "GPU lease for {lease_id} is held by pid {held_by}; another extractor is \
                 running. Wait for it, or pass --no-lease to bypass coordination."
            );
        }
    }

    // #kgctxinject: when extracting into a graph.db that already holds
    // entities, build a bounded known-entity context pack from the current
    // graph so the model reconciles against canonical stable ids instead of
    // re-inventing them. Loading is read-only and bounded by ContextPackConfig.
    // `kg refresh --apply` re-extracts changed files through this same path, so
    // this seam also delivers graph-aware incremental re-extraction
    // (#kgctxincremental) — the re-extract reconciles rather than duplicates.
    let existing_projection = if no_context {
        None
    } else {
        match graph_db.as_deref() {
            Some(db) if db.exists() => {
                let store = tsift_sqlite::SqliteGraphStore::open_read_only_resilient(db)
                    .with_context(|| {
                        format!(
                            "opening graph.db read-only at {} for context pack",
                            db.display()
                        )
                    })?;
                let nodes = store
                    .all_nodes()
                    .context("reading graph_nodes for context pack")?;
                // Only inject when the graph has entities to reconcile against;
                // a fresh/empty graph has nothing to offer and stays byte-for-byte
                // identical to the no-context prompt path.
                if nodes.iter().any(|n| n.kind == "semantic_entity") {
                    let edges = store
                        .all_edges()
                        .context("reading graph_edges for context pack")?;
                    Some(GraphProjection { nodes, edges })
                } else {
                    None
                }
            }
            _ => None,
        }
    };
    let context_entities = existing_projection
        .as_ref()
        .map(|proj| proj.nodes.iter().filter(|n| n.kind == "semantic_entity").count());
    let context_source = existing_projection
        .as_ref()
        .map(|proj| ChunkContextSource::new(proj, ContextPackConfig::default()));

    let document = KgInputDocument::new(KgInputKind::Source, &source_ref, &text);
    let report = extract_documents_to_projection_with_context(
        &[document],
        &extractor,
        ChunkingConfig::default(),
        context_source.as_ref(),
    )
    .context("KG extraction pipeline failed")?;

    let provider_id = report
        .extracted_chunks
        .first()
        .map(|c| c.chunk.id.as_str())
        .unwrap_or("")
        .to_string();
    let node_total = report.projection.nodes.len();
    let node_preview: Vec<(String, String)> = report
        .projection
        .nodes
        .iter()
        .take(20)
        .map(|n| (n.label.clone(), n.kind.clone()))
        .collect();
    let entity_count = node_total;
    let relation_count = report.projection.edges.len();

    let upsert = if let Some(graph_db) = graph_db.as_deref() {
        Some(upsert_kg_projection_sqlite(graph_db, &report.projection).context(format!(
            "upserting KG projection into {}",
            graph_db.display()
        ))?)
    } else {
        None
    };

    // #kgleasewire: release the lease; reference-counted unload when this
    // extract dropped the last live reference (unless --keep-loaded).
    let mut unloaded = None;
    if let Some(ref lease_id) = lease_profile {
        let release = tsift_local_model::release_lease(
            lease_id,
            holder_pid,
            tsift_local_model::current_unix_seconds(),
            &lease_path,
        )
        .with_context(|| format!("releasing GPU lease for {lease_id}"))?;
        if !keep_loaded && release.remaining_holders == 0 {
            let endpoint = resolve_ollama_host_for_unload(host.as_deref());
            let outcome = tsift_local_model::unload_model_at(&endpoint, &resolved_model);
            unloaded = Some(format!(
                "Unloaded {} (last lease reference released): {}",
                resolved_model, outcome.outcome
            ));
        }
    }

    Ok(KgExtractOutcome {
        provider_id,
        model: extractor.model().to_string(),
        host: extractor.host().to_string(),
        source_ref,
        chunks: report.chunks.len(),
        entities: entity_count,
        relations: relation_count,
        upsert,
        node_preview,
        node_total,
        unloaded,
        context_entities,
    })
}

pub(crate) fn cmd_kg_extract(args: KgExtractArgs) -> Result<()> {
    let json = args.json;
    let outcome = run_kg_extract(args)?;
    emit_kg_extract_outcome(&outcome, json);
    Ok(())
}

fn emit_kg_extract_outcome(outcome: &KgExtractOutcome, json: bool) {
    if json {
        let payload = serde_json::json!({
            "provider_id": outcome.provider_id,
            "model": outcome.model,
            "host": outcome.host,
            "source_ref": outcome.source_ref,
            "chunks": outcome.chunks,
            "entities": outcome.entities,
            "relations": outcome.relations,
            "upsert": outcome.upsert,
            "context_entities": outcome.context_entities,
        });
        println!(
            "{}",
            serde_json::to_string_pretty(&payload).unwrap_or_else(|_| "{}".into())
        );
        return;
    }
    println!("KG extraction");
    println!("Model: {} ({})", outcome.model, outcome.host);
    println!(
        "Chunks: {}  Entities: {}  Relations: {}",
        outcome.chunks, outcome.entities, outcome.relations
    );
    if let Some(known) = outcome.context_entities {
        println!("Context: {known} known entities injected (graph-aware reconciliation)");
    }
    for (label, kind) in &outcome.node_preview {
        println!("  - {} [{}]", label, kind);
    }
    if outcome.node_total > outcome.node_preview.len() {
        println!(
            "  ... ({} more)",
            outcome.node_total - outcome.node_preview.len()
        );
    }
    if let Some(ref upsert) = outcome.upsert {
        println!("Upserted into: {}", upsert.graph_db);
    }
    if let Some(ref notice) = outcome.unloaded {
        println!("{}", notice);
    }
}

// =============================================================================
// refresh (#kgextractrefresh — on-demand staleness detection)
// =============================================================================

/// Staleness of one recorded `kg_source` against the current working tree.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum RefreshStatus {
    /// File content differs from the hash recorded at extraction.
    Stale,
    /// File content matches the recorded hash.
    Unchanged,
    /// `source_ref` is not a readable file (deleted, moved, or a non-path label).
    Missing,
    /// Extracted before `#kgextractrefresh` recorded content hashes; staleness
    /// is unknown, so a refresh is recommended.
    NoRecordedHash,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(crate) struct RefreshEntry {
    pub source_ref: String,
    pub status: RefreshStatus,
    pub recorded_hash: Option<String>,
    pub current_hash: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(crate) struct RefreshPlan {
    pub entries: Vec<RefreshEntry>,
}

impl RefreshPlan {
    /// Entries that warrant re-extraction (changed or unknown-staleness).
    pub fn needs_refresh(&self) -> Vec<&RefreshEntry> {
        self.entries
            .iter()
            .filter(|e| matches!(e.status, RefreshStatus::Stale | RefreshStatus::NoRecordedHash))
            .collect()
    }

    /// Entries that `refresh --apply` will actually re-extract: those that
    /// need refresh AND whose `source_ref` is still a readable file (a current
    /// content hash exists). Stale entries always qualify; a `NoRecordedHash`
    /// entry qualifies only when the file is currently readable. Entries whose
    /// file is missing are skipped (#kgrefreshapply).
    pub fn apply_targets(&self) -> Vec<&RefreshEntry> {
        self.needs_refresh()
            .into_iter()
            .filter(|e| e.current_hash.is_some())
            .collect()
    }
}

/// Pure staleness planner: classify each recorded `(source_ref, recorded_hash)`
/// against the current content hash returned by `current_hash` (`None` when the
/// source is not a readable file).
pub(crate) fn plan_refresh(
    recorded: &[(String, Option<String>)],
    current_hash: impl Fn(&str) -> Option<String>,
) -> RefreshPlan {
    let mut entries = Vec::with_capacity(recorded.len());
    for (source_ref, recorded_hash) in recorded {
        let current = current_hash(source_ref);
        let status = match (recorded_hash, &current) {
            (None, _) => RefreshStatus::NoRecordedHash,
            (Some(_), None) => RefreshStatus::Missing,
            (Some(r), Some(c)) if r == c => RefreshStatus::Unchanged,
            (Some(_), Some(_)) => RefreshStatus::Stale,
        };
        entries.push(RefreshEntry {
            source_ref: source_ref.clone(),
            status,
            recorded_hash: recorded_hash.clone(),
            current_hash: current,
        });
    }
    RefreshPlan { entries }
}

/// Blake3 hash of a file's contents, or `None` when it is not a readable file
/// (matches the recorded `source_content_hash` written at extraction).
fn file_content_hash(source_ref: &str) -> Option<String> {
    let path = Path::new(source_ref);
    if !path.is_file() {
        return None;
    }
    std::fs::read(path)
        .ok()
        .map(|bytes| blake3::hash(&bytes).to_hex().to_string())
}

pub(crate) fn cmd_kg_refresh(args: KgRefreshArgs) -> Result<()> {
    let KgRefreshArgs {
        graph_db,
        json,
        apply,
        profile,
        model,
        host,
        no_lease,
        idle_ttl_seconds,
        keep_loaded,
        lease_file,
        no_context,
    } = args;
    let db_path = graph_db.unwrap_or_else(|| PathBuf::from(DEFAULT_GRAPH_DB_RELATIVE));
    if !db_path.exists() {
        if json {
            println!(
                "{}",
                serde_json::to_string_pretty(&serde_json::json!({
                    "graph_db": db_path.display().to_string(),
                    "exists": false,
                    "apply": apply,
                    "entries": [],
                }))?
            );
        } else {
            println!(
                "no graph.db at {} — run `tsift kg extract --graph-db <path>` first",
                db_path.display()
            );
        }
        return Ok(());
    }

    let store = tsift_sqlite::SqliteGraphStore::open_read_only_resilient(&db_path)
        .with_context(|| format!("opening graph.db read-only at {}", db_path.display()))?;
    let nodes = store
        .all_nodes()
        .with_context(|| "reading graph_nodes for kg refresh")?;
    let recorded: Vec<(String, Option<String>)> = nodes
        .iter()
        .filter(|n| n.kind == "kg_source")
        .map(|n| {
            let source_ref = n
                .properties
                .get("source_ref")
                .cloned()
                .unwrap_or_else(|| n.label.clone());
            let recorded_hash = n.properties.get("source_content_hash").cloned();
            (source_ref, recorded_hash)
        })
        .collect();

    let plan = plan_refresh(&recorded, file_content_hash);
    let needs = plan.needs_refresh();

    // Read-only staleness plan (#kgextractrefresh) — the default when `--apply`
    // is absent.
    if !apply {
        if json {
            println!(
                "{}",
                serde_json::to_string_pretty(&serde_json::json!({
                    "graph_db": db_path.display().to_string(),
                    "exists": true,
                    "sources": plan.entries.len(),
                    "needs_refresh": needs.len(),
                    "entries": plan.entries,
                }))?
            );
        } else {
            println!("KG refresh plan ({})", db_path.display());
            println!(
                "Sources: {}  Needs refresh: {}",
                plan.entries.len(),
                needs.len()
            );
            for entry in &plan.entries {
                println!("  - {} [{:?}]", entry.source_ref, entry.status);
            }
            if !needs.is_empty() {
                println!("\nRe-extract stale/unknown sources with:");
                println!(
                    "  tsift kg refresh --apply --graph-db {}   (or per-source `tsift kg extract`)",
                    db_path.display()
                );
                for entry in &needs {
                    println!(
                        "  tsift kg extract --input {0} --source-ref {0} --graph-db {1}",
                        entry.source_ref,
                        db_path.display()
                    );
                }
            }
        }
        return Ok(());
    }

    // #kgrefreshapply: auto re-extract every stale / no_recorded_hash source
    // whose file is still readable, reusing the lease-aware `run_kg_extract`
    // path. Per-source output is collected into a unified summary so JSON stays
    // a single document and the human path stays readable.
    let targets = plan.apply_targets();
    let skipped: Vec<&RefreshEntry> = needs
        .iter()
        .copied()
        .filter(|e| e.current_hash.is_none())
        .collect();

    if !json {
        println!("KG refresh --apply ({})", db_path.display());
        println!(
            "Sources: {}  Needs refresh: {}  Re-extracting: {}  Skipping (no file): {}",
            plan.entries.len(),
            needs.len(),
            targets.len(),
            skipped.len()
        );
        for entry in &plan.entries {
            println!("  - {} [{:?}]", entry.source_ref, entry.status);
        }
    }

    let mut outcomes: Vec<KgExtractOutcome> = Vec::with_capacity(targets.len());
    let mut errors: Vec<(String, String)> = Vec::new();
    for entry in &targets {
        if !json {
            println!("\nRe-extracting {} ...", entry.source_ref);
        }
        let extract_args = KgExtractArgs {
            profile: profile.clone(),
            model: model.clone(),
            host: host.clone(),
            input: Some(PathBuf::from(&entry.source_ref)),
            source_ref: Some(entry.source_ref.clone()),
            graph_db: Some(db_path.clone()),
            no_lease,
            idle_ttl_seconds,
            keep_loaded,
            lease_file: lease_file.clone(),
            no_context,
            json: false,
        };
        match run_kg_extract(extract_args) {
            Ok(outcome) => {
                if !json {
                    emit_kg_extract_outcome(&outcome, false);
                }
                outcomes.push(outcome);
            }
            Err(err) => errors.push((entry.source_ref.clone(), format!("{err:#}"))),
        }
    }

    if json {
        let errors_json: Vec<_> = errors
            .iter()
            .map(|(s, e)| serde_json::json!({ "source_ref": s, "error": e }))
            .collect();
        let skipped_json: Vec<_> = skipped
            .iter()
            .map(|e| serde_json::json!({ "source_ref": e.source_ref, "status": e.status }))
            .collect();
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({
                "graph_db": db_path.display().to_string(),
                "exists": true,
                "apply": true,
                "sources": plan.entries.len(),
                "needs_refresh": needs.len(),
                "applied": outcomes.len(),
                "failed": errors.len(),
                "entries": plan.entries,
                "results": outcomes,
                "errors": errors_json,
                "skipped": skipped_json,
            }))?
        );
    } else {
        println!(
            "\nRefresh apply complete: {} re-extracted, {} failed, {} skipped",
            outcomes.len(),
            errors.len(),
            skipped.len()
        );
        for (src, err) in &errors {
            println!("  FAILED {}: {}", src, err);
        }
        for entry in &skipped {
            println!("  SKIPPED {} (no readable file)", entry.source_ref);
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

// =============================================================================
// status (#kgcliext — spec local-kg-model.md line 31-32)
// =============================================================================

#[derive(Debug, Serialize)]
struct KgStatusReport {
    graph_db: String,
    exists: bool,
    total_nodes: usize,
    total_edges: usize,
    nodes_by_kind: BTreeMap<String, usize>,
    edges_by_kind: BTreeMap<String, usize>,
}

pub(crate) fn cmd_kg_status(graph_db: Option<PathBuf>, json: bool) -> Result<()> {
    let db_path = graph_db
        .unwrap_or_else(|| PathBuf::from(DEFAULT_GRAPH_DB_RELATIVE));
    let report = build_kg_status_report(&db_path)?;
    emit_status(&report, json);
    Ok(())
}

fn build_kg_status_report(db_path: &Path) -> Result<KgStatusReport> {
    let db_display = db_path.display().to_string();
    if !db_path.exists() {
        return Ok(KgStatusReport {
            graph_db: db_display,
            exists: false,
            total_nodes: 0,
            total_edges: 0,
            nodes_by_kind: BTreeMap::new(),
            edges_by_kind: BTreeMap::new(),
        });
    }

    let store = tsift_sqlite::SqliteGraphStore::open_read_only_resilient(db_path)
        .with_context(|| format!("opening graph.db read-only at {}", db_path.display()))?;
    let nodes = store
        .all_nodes()
        .with_context(|| "reading graph_nodes for kg status")?;
    let edges = store
        .all_edges()
        .with_context(|| "reading graph_edges for kg status")?;

    let mut nodes_by_kind = BTreeMap::new();
    for node in &nodes {
        *nodes_by_kind.entry(node.kind.clone()).or_insert(0) += 1;
    }
    let mut edges_by_kind = BTreeMap::new();
    for edge in &edges {
        *edges_by_kind.entry(edge.kind.clone()).or_insert(0) += 1;
    }

    Ok(KgStatusReport {
        graph_db: db_display,
        exists: true,
        total_nodes: nodes.len(),
        total_edges: edges.len(),
        nodes_by_kind,
        edges_by_kind,
    })
}

fn emit_status(report: &KgStatusReport, json: bool) {
    if json {
        println!("{}", serde_json::to_string_pretty(report).unwrap_or_else(|_| "{}".into()));
        return;
    }
    println!("KG status");
    println!("Graph DB: {}", report.graph_db);
    if !report.exists {
        println!("  (no graph.db at this path — run `tsift kg extract --graph-db <path>` to populate)");
        return;
    }
    println!(
        "Nodes: {}   Edges: {}",
        report.total_nodes, report.total_edges
    );
    if !report.nodes_by_kind.is_empty() {
        println!("Nodes by kind (top 10):");
        let mut node_kinds: Vec<_> = report.nodes_by_kind.iter().collect();
        node_kinds.sort_by(|a, b| b.1.cmp(a.1));
        for (kind, count) in node_kinds.into_iter().take(10) {
            println!("  {kind}: {count}");
        }
    }
    if !report.edges_by_kind.is_empty() {
        println!("Edges by kind (top 10):");
        let mut edge_kinds: Vec<_> = report.edges_by_kind.iter().collect();
        edge_kinds.sort_by(|a, b| b.1.cmp(a.1));
        for (kind, count) in edge_kinds.into_iter().take(10) {
            println!("  {kind}: {count}");
        }
    }
}

// =============================================================================
// evidence (#kgadactivate — agent-doc's KG read seam per spec line 29-30)
// =============================================================================

pub(crate) fn cmd_kg_evidence(
    symbol: Option<String>,
    kind: Option<String>,
    limit: usize,
    graph_db: Option<PathBuf>,
    json: bool,
) -> Result<()> {
    let db_path = graph_db
        .unwrap_or_else(|| PathBuf::from(DEFAULT_GRAPH_DB_RELATIVE));
    let mut query = tsift_agent_doc::graph_evidence::GraphEvidenceQuery::default().with_limit(limit);
    if let Some(symbol) = symbol {
        query = query.with_symbol(symbol);
    }
    if let Some(kind) = kind {
        query = query.with_kind(kind);
    }
    let report =
        tsift_agent_doc::graph_evidence::read_graph_evidence_from_db(&db_path, &query)?;

    if json {
        println!("{}", serde_json::to_string_pretty(&report)?);
        return Ok(());
    }

    println!("KG evidence");
    println!("Graph DB: {}", report.graph_db);
    if !report.exists {
        println!("  (no graph.db at this path — run `tsift kg extract --graph-db <path>` to populate)");
        return Ok(());
    }
    println!(
        "Total: {} nodes / {} edges.  Matched: {} (limit {})",
        report.total_nodes_in_db,
        report.total_edges_in_db,
        report.matched_nodes.len(),
        report.query.limit,
    );
    if let Some(symbol) = &report.query.symbol {
        println!("Symbol filter: {symbol}");
    }
    if let Some(kind) = &report.query.kind {
        println!("Kind filter: {kind}");
    }
    if report.matched_nodes.is_empty() {
        println!("(no matches)");
        return Ok(());
    }
    for node in &report.matched_nodes {
        let sources = if node.provenance_systems.is_empty() {
            String::new()
        } else {
            format!(" [{}]", node.provenance_systems.join(", "))
        };
        let refs = if node.source_refs.is_empty() {
            String::new()
        } else {
            format!(" — {}", node.source_refs.join(", "))
        };
        println!(
            "  - {} [{}]{} ({} edges){}",
            node.label, node.kind, sources, node.incident_edge_count, refs,
        );
    }
    Ok(())
}

// =============================================================================
// unload (#kgunloadpost — build_unload_actions owns execution, called from CLI)
// =============================================================================

#[derive(Debug, Serialize)]
struct KgUnloadReport {
    profile_id: String,
    model_tag: String,
    endpoint: String,
    actions: Vec<tsift_local_model::UnloadActionResult>,
}

pub(crate) fn cmd_kg_unload(
    profile: Option<String>,
    model: Option<String>,
    host: Option<String>,
    json: bool,
) -> Result<()> {
    let profile_id = profile.unwrap_or_else(|| DEFAULT_PROFILE_ID.to_string());
    let profile_handle = profile_by_id(&profile_id).ok_or_else(|| {
        anyhow::anyhow!(
            "unknown tsift local-model profile `{profile_id}`. Use --model <ollama-tag> or one of: {}",
            list_profile_ids().join(", ")
        )
    })?;
    let model_tag = model.unwrap_or_else(|| profile_handle.model_ref.to_string());
    let endpoint = resolve_ollama_host_for_unload(host.as_deref());

    // The planner owns execution (#kgunloadpost): build_unload_actions still
    // plans, and execute_unload_actions POSTs the ollama keep_alive:0 with
    // the resolved model tag (so --model override actually takes effect).
    let actions = tsift_local_model::build_unload_actions(
        &profile_handle,
        Some(endpoint.as_str()),
        None,
    );
    let results = tsift_local_model::execute_unload_actions(&actions, &model_tag);

    let report = KgUnloadReport {
        profile_id,
        model_tag,
        endpoint,
        actions: results,
    };

    if json {
        println!("{}", serde_json::to_string_pretty(&report)?);
    } else {
        println!("KG unload");
        println!("Profile: {} ({})", report.profile_id, report.model_tag);
        println!("Endpoint: {}", report.endpoint);
        for action in &report.actions {
            println!(
                "  - {}: {} ({})",
                action.label,
                if action.executed { "executed" } else { "skipped" },
                action.outcome
            );
        }
    }
    Ok(())
}

fn resolve_ollama_host_for_unload(host_override: Option<&str>) -> String {
    if let Some(host) = host_override
        && !host.trim().is_empty()
    {
        return host.trim_end_matches('/').to_string();
    }
    tsift_local_model::resolve_provider_endpoint(
        &tsift_local_model::UnloadStrategy::OllamaKeepAliveZero,
        None,
    )
    .trim_end_matches('/')
    .to_string()
}

// =============================================================================
// smoke (spec local-kg-model.md line 31-32)
// =============================================================================

#[derive(Debug, Serialize)]
struct KgSmokeReport {
    profile_id: Option<String>,
    model_tag: String,
    host: String,
    chunks: usize,
    entities: usize,
    relations: usize,
    unloaded: bool,
}

pub(crate) fn cmd_kg_smoke(
    profile: Option<String>,
    model: Option<String>,
    host: Option<String>,
    unload_after: bool,
    json: bool,
) -> Result<()> {
    let resolved_model = resolve_model_tag(profile.as_deref(), model.as_deref())?;
    let extractor = match host.as_deref() {
        Some(host) => OllamaKgExtractor::new(&resolved_model).with_host(host),
        None => OllamaKgExtractor::new(&resolved_model),
    };
    let document =
        KgInputDocument::new(KgInputKind::Source, SMOKE_SOURCE_REF, SMOKE_SAMPLE_TEXT);
    let report = extract_documents_to_projection(&[document], &extractor, ChunkingConfig::default())
        .context("KG smoke extraction failed — is the Ollama server running and the model pulled?")?;
    let entities = report.projection.nodes.len();
    let relations = report.projection.edges.len();

    let mut unloaded = false;
    if unload_after {
        let outcome = tsift_local_model::unload_model_at(extractor.host(), &resolved_model);
        if outcome.executed {
            unloaded = true;
        } else {
            eprintln!("kg smoke: unload after run failed: {}", outcome.outcome);
        }
    }

    let summary = KgSmokeReport {
        profile_id: profile.clone(),
        model_tag: extractor.model().to_string(),
        host: extractor.host().to_string(),
        chunks: report.chunks.len(),
        entities,
        relations,
        unloaded,
    };

    if json {
        println!("{}", serde_json::to_string_pretty(&summary)?);
    } else {
        println!("KG smoke");
        println!(
            "Model: {} ({})",
            summary.model_tag, summary.host
        );
        println!(
            "Chunks: {}   Entities: {}   Relations: {}",
            summary.chunks, summary.entities, summary.relations
        );
        if summary.unloaded {
            println!("Unloaded: yes");
        }
        println!("OK");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;
    use tsift_core::{GraphEdge, GraphNode, GraphProjection};
    use tsift_sqlite::SqliteGraphStore;

    #[test]
    fn plan_refresh_classifies_each_source() {
        let recorded = vec![
            ("a.md".to_string(), Some("hash-a".to_string())), // unchanged
            ("b.md".to_string(), Some("hash-b-old".to_string())), // stale
            ("c.md".to_string(), Some("hash-c".to_string())), // missing (no current)
            ("d.md".to_string(), None),                       // no recorded hash
        ];
        let current = |s: &str| match s {
            "a.md" => Some("hash-a".to_string()),
            "b.md" => Some("hash-b-new".to_string()),
            "c.md" => None,
            "d.md" => Some("hash-d".to_string()),
            _ => None,
        };
        let plan = plan_refresh(&recorded, current);
        assert_eq!(plan.entries[0].status, RefreshStatus::Unchanged);
        assert_eq!(plan.entries[1].status, RefreshStatus::Stale);
        assert_eq!(plan.entries[2].status, RefreshStatus::Missing);
        assert_eq!(plan.entries[3].status, RefreshStatus::NoRecordedHash);
        // Stale + NoRecordedHash warrant re-extraction; Unchanged + Missing do not.
        let needs: Vec<_> = plan.needs_refresh().iter().map(|e| e.source_ref.clone()).collect();
        assert_eq!(needs, vec!["b.md".to_string(), "d.md".to_string()]);
    }

    #[test]
    fn plan_refresh_empty_when_no_sources() {
        let plan = plan_refresh(&[], |_| None);
        assert!(plan.entries.is_empty());
        assert!(plan.needs_refresh().is_empty());
        assert!(plan.apply_targets().is_empty());
    }

    #[test]
    fn apply_targets_filters_to_readable_needs_refresh_sources() {
        // Stale (file present) + NoRecordedHash-with-file qualify for --apply;
        // Missing and NoRecordedHash-without-file are skipped even though they
        // "need refresh", because there is no readable file to re-extract.
        let recorded = vec![
            ("unchanged.md".to_string(), Some("h".to_string())),
            ("stale.md".to_string(), Some("h-old".to_string())),
            ("missing.md".to_string(), Some("h".to_string())),
            ("nohash_present.md".to_string(), None),
            ("nohash_gone.md".to_string(), None),
        ];
        let current = |s: &str| match s {
            "unchanged.md" => Some("h".to_string()),
            "stale.md" => Some("h-new".to_string()),
            "missing.md" => None,
            "nohash_present.md" => Some("h".to_string()),
            "nohash_gone.md" => None,
            _ => None,
        };
        let plan = plan_refresh(&recorded, current);
        let targets: Vec<_> = plan
            .apply_targets()
            .into_iter()
            .map(|e| e.source_ref.clone())
            .collect();
        assert_eq!(
            targets,
            vec!["stale.md".to_string(), "nohash_present.md".to_string()]
        );
    }

    #[test]
    fn resolve_lease_profile_id_defaults_when_no_profile_or_model() {
        assert_eq!(
            resolve_lease_profile_id(None, None).as_deref(),
            Some(DEFAULT_PROFILE_ID)
        );
    }

    #[test]
    fn resolve_lease_profile_id_uses_explicit_profile() {
        assert_eq!(
            resolve_lease_profile_id(Some("qwen3-32b-q4"), None).as_deref(),
            Some("qwen3-32b-q4")
        );
    }

    #[test]
    fn resolve_lease_profile_id_none_when_explicit_model_bypasses_profiles() {
        // An explicit --model bypasses profile resolution, so there is no
        // profile to lease — extract proceeds without GPU lease coordination.
        assert_eq!(resolve_lease_profile_id(None, Some("some-ollama-tag")), None);
        assert_eq!(
            resolve_lease_profile_id(Some("qwen3-32b-q4"), Some("some-ollama-tag")),
            None
        );
    }

    #[test]
    fn kg_status_reports_missing_db_cleanly() {
        let dir = TempDir::new().unwrap();
        let missing = dir.path().join("does-not-exist.db");
        let report = build_kg_status_report(&missing).expect("missing db should not error");
        assert!(!report.exists);
        assert_eq!(report.total_nodes, 0);
        assert_eq!(report.total_edges, 0);
        assert!(report.nodes_by_kind.is_empty());
        assert!(report.edges_by_kind.is_empty());
        assert!(report.graph_db.contains("does-not-exist.db"));
    }

    #[test]
    fn kg_status_counts_nodes_and_edges_by_kind() {
        let dir = TempDir::new().unwrap();
        let db_path = dir.path().join("graph.db");
        let mut store = SqliteGraphStore::open(&db_path).expect("open writable store");

        let mut projection = GraphProjection::default();
        projection.nodes.push(
            GraphNode::new("n:kg-1", "kg_source", "tsift-kg")
                .with_property("provider", "tsift-kg"),
        );
        projection.nodes.push(
            GraphNode::new("n:kg-2", "kg_source", "OllamaKgExtractor")
                .with_property("provider", "tsift-kg"),
        );
        projection.nodes.push(GraphNode::new("n:other", "concept", "lease"));
        projection.edges.push(GraphEdge::new("n:kg-1", "n:kg-2", "calls"));
        projection.edges.push(GraphEdge::new("n:kg-1", "n:other", "related_to"));
        store
            .upsert_projection(&projection)
            .expect("upsert projection");

        let report = build_kg_status_report(&db_path).expect("populated db status");
        assert!(report.exists);
        assert_eq!(report.total_nodes, 3);
        assert_eq!(report.total_edges, 2);
        assert_eq!(report.nodes_by_kind.get("kg_source"), Some(&2));
        assert_eq!(report.nodes_by_kind.get("concept"), Some(&1));
        assert_eq!(report.edges_by_kind.get("calls"), Some(&1));
        assert_eq!(report.edges_by_kind.get("related_to"), Some(&1));
    }

    #[test]
    fn resolve_ollama_host_for_unload_uses_explicit_override() {
        let host = resolve_ollama_host_for_unload(Some("http://192.168.1.17:11434"));
        assert_eq!(host, "http://192.168.1.17:11434");
    }

    #[test]
    fn resolve_ollama_host_for_unload_falls_back_when_blank() {
        let host = resolve_ollama_host_for_unload(Some("   "));
        // Resolves to the default ollama endpoint exposed by tsift-local-model.
        assert!(!host.is_empty());
        assert!(host.starts_with("http"));
    }
}
