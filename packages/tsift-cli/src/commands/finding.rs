//! `tsift finding` — Phase 1 of the Findings Graph Layer (#trt1p1).
//!
//! Findings are *authored* knowledge (design decisions, invariants, gotchas)
//! anchored to code. Unlike the derived code projection at `.tsift/graph.db`,
//! findings must survive a code reindex, so they live in a dedicated
//! `.tsift/findings.db` opened through the same provider-neutral
//! [`SqliteGraphStore`]. The code-projection refresh
//! (`replace_projection_with_version`) tombstones non-projected rows; it never
//! touches `findings.db`, which is the durability guarantee this layer relies
//! on. See `specs/graph.md` § Findings Graph Layer.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use anyhow::{Result, bail};
use serde::Serialize;
use tsift_index::config::Config as IndexConfig;
use tsift_index::index;
use tsift_quality::lint;
use tsift_sqlite::{
    GraphEdge as SubstrateGraphEdge, GraphFreshness, GraphNode as SubstrateGraphNode,
    GraphProvenance, GraphStore, SqliteGraphStore,
};

use crate::open_index_db;

/// Dedicated findings store, independent of the rebuildable code projection.
pub(crate) fn findings_db_path(root: &Path) -> std::path::PathBuf {
    root.join(".tsift/findings.db")
}

/// Resolved anchor for a finding: a stable graph node plus the freshness
/// watermark (content hash) captured at the moment the finding is authored.
struct ResolvedAnchor {
    /// Stable graph node id for the anchor (`code_symbol:*` or `file:*`).
    node_id: String,
    /// Graph node kind: `code_symbol` or `file`.
    node_kind: &'static str,
    /// Human label (symbol name or file path).
    label: String,
    /// Content hash of the anchored content at capture time, when resolvable.
    watermark: Option<String>,
    /// Properties to persist on the anchor node (e.g. `file`, `line`).
    properties: Vec<(String, String)>,
}

/// Hash the bytes of a symbol's body span (preferred) or its full file as the
/// staleness watermark. Returns `None` when the source cannot be read.
fn symbol_watermark(root: &Path, symbol: &index::StoredSymbol) -> Option<String> {
    let source_path = crate::resolve_source_file(root, Path::new(&symbol.file)).ok()?;
    let bytes = std::fs::read(&source_path).ok()?;
    // Prefer the precise body span so unrelated edits in the same file do not
    // mark every finding in that file stale. Fall back to the whole file.
    if let (Some(start), Some(end)) = (symbol.start_byte, symbol.end_byte)
        && start >= 0
        && end >= start
        && (end as usize) <= bytes.len()
    {
        return Some(tsift_graph::source_content_hash(&bytes[start as usize..end as usize]));
    }
    Some(tsift_graph::source_content_hash(&bytes))
}

/// Hash a file's full contents as the staleness watermark.
fn file_watermark(root: &Path, file: &str) -> Option<String> {
    let source_path = crate::resolve_source_file(root, Path::new(file)).ok()?;
    let bytes = std::fs::read(&source_path).ok()?;
    Some(tsift_graph::source_content_hash(&bytes))
}

/// Resolve `--about` against the code index when possible (symbol first, then
/// file). Anchoring is by stable handle, never by line number, so a finding
/// survives reformatting.
fn resolve_anchor(root: &Path, about: &str, scope: Option<&str>) -> Result<ResolvedAnchor> {
    // 1. Try to resolve as an indexed symbol.
    if let Ok(db) = open_index_db(root, scope)
        && let Ok(symbols) = db.symbol_info(about)
        && let Some(symbol) = symbols.into_iter().next()
    {
        let node_id = format!("code_symbol:{}", about);
        let watermark = symbol_watermark(root, &symbol);
        let mut properties = vec![
            ("name".to_string(), symbol.name.clone()),
            ("file".to_string(), symbol.file.clone()),
            ("line".to_string(), symbol.line.to_string()),
        ];
        if !symbol.kind.is_empty() {
            properties.push(("symbol_kind".to_string(), symbol.kind.clone()));
        }
        return Ok(ResolvedAnchor {
            node_id,
            node_kind: "code_symbol",
            label: about.to_string(),
            watermark,
            properties,
        });
    }

    // 2. Fall back to a file anchor.
    let watermark = file_watermark(root, about);
    let node_id = format!("file:{}", about);
    Ok(ResolvedAnchor {
        node_id,
        node_kind: "file",
        label: about.to_string(),
        watermark,
        properties: vec![("path".to_string(), about.to_string())],
    })
}

/// Deterministic, content-derived finding id so the same authored finding is
/// idempotent across re-runs.
fn finding_id(kind: &str, title: &str, about: &str) -> String {
    let raw = format!("{kind}\u{1f}{title}\u{1f}{about}");
    format!("finding:{}", blake3::hash(raw.as_bytes()).to_hex())
}

fn now_unix() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|dur| dur.as_secs() as i64)
        .unwrap_or(0)
}

#[derive(Serialize)]
struct FindingAddReport {
    id: String,
    kind: String,
    title: String,
    status: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    confidence: Option<f64>,
    about: String,
    anchor_node: String,
    anchor_kind: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    watermark: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    relates_to: Option<String>,
    db: String,
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn cmd_finding_add(
    path: &Path,
    kind: &str,
    title: &str,
    body: &str,
    about: &str,
    confidence: Option<f64>,
    status: &str,
    relates: Option<&str>,
    scope: Option<&str>,
    json_output: bool,
    pretty: bool,
) -> Result<()> {
    if !matches!(kind, "finding" | "decision" | "note") {
        bail!("--kind must be one of finding|decision|note (got {kind})");
    }
    if !matches!(status, "draft" | "trusted") {
        bail!("--status must be one of draft|trusted (got {status})");
    }
    if let Some(confidence) = confidence
        && !(0.0..=1.0).contains(&confidence)
    {
        bail!("--confidence must be between 0.0 and 1.0 (got {confidence})");
    }

    let root = lint::resolve_project_root_or_canonical_path(path)?;
    let anchor = resolve_anchor(&root, about, scope)?;

    let db_path = findings_db_path(&root);
    let store = SqliteGraphStore::open(&db_path)?;

    let observed = now_unix();

    // 1. Upsert the anchor node (required before the FK-checked `concerns`
    //    edge). The anchor is a lightweight reference into the code graph,
    //    re-derived on each add so it tracks the current handle.
    let mut anchor_node = SubstrateGraphNode::new(
        anchor.node_id.clone(),
        anchor.node_kind,
        anchor.label.clone(),
    );
    for (key, value) in &anchor.properties {
        anchor_node = anchor_node.with_property(key.clone(), value.clone());
    }
    store.upsert_node(&anchor_node)?;

    // 2. Build and upsert the finding node.
    let id = finding_id(kind, title, about);
    let mut node = SubstrateGraphNode::new(id.clone(), kind, title.to_string())
        .with_property("title", title.to_string())
        .with_property("body", body.to_string())
        .with_property("status", status.to_string())
        .with_property("author", "agent")
        .with_property("about", about.to_string())
        .with_property("anchor_node", anchor.node_id.clone())
        .with_property("anchor_kind", anchor.node_kind);
    if let Some(confidence) = confidence {
        node = node.with_property("confidence", format!("{confidence}"));
    }
    if let Some(watermark) = &anchor.watermark {
        node = node.with_property("watermark", watermark.clone());
    }
    let mut provenance = GraphProvenance::new("findings", format!("finding-add:{about}"));
    if let Some(watermark) = &anchor.watermark {
        provenance = provenance.with_content_hash(watermark.clone());
    }
    node = node.with_provenance(provenance);
    node = node.with_freshness(GraphFreshness {
        content_hash: anchor.watermark.clone(),
        observed_at_unix: Some(observed),
    });
    store.upsert_node(&node)?;

    // 3. `concerns` edge: finding -> anchor, also stamped with the watermark.
    let mut concerns = SubstrateGraphEdge::new(id.clone(), anchor.node_id.clone(), "concerns");
    if let Some(watermark) = &anchor.watermark {
        concerns = concerns
            .with_property("watermark", watermark.clone())
            .with_freshness(GraphFreshness {
                content_hash: Some(watermark.clone()),
                observed_at_unix: Some(observed),
            });
    }
    store.upsert_edge(&concerns)?;

    // 4. Optional `relates_to` edge: finding -> finding.
    if let Some(relates) = relates {
        if store.node(relates)?.is_none() {
            bail!("--relates target finding not found in {}: {relates}", db_path.display());
        }
        let relates_edge = SubstrateGraphEdge::new(id.clone(), relates.to_string(), "relates_to");
        store.upsert_edge(&relates_edge)?;
    }

    let report = FindingAddReport {
        id: id.clone(),
        kind: kind.to_string(),
        title: title.to_string(),
        status: status.to_string(),
        confidence,
        about: about.to_string(),
        anchor_node: anchor.node_id.clone(),
        anchor_kind: anchor.node_kind.to_string(),
        watermark: anchor.watermark.clone(),
        relates_to: relates.map(|relates| relates.to_string()),
        db: db_path.display().to_string(),
    };

    if json_output {
        let rendered = if pretty {
            serde_json::to_string_pretty(&report)?
        } else {
            serde_json::to_string(&report)?
        };
        println!("{rendered}");
    } else {
        println!("added {} [{}] {}", report.id, report.kind, report.title);
        println!("  anchor: {} ({})", report.anchor_node, report.anchor_kind);
        match &report.watermark {
            Some(watermark) => println!("  watermark: {watermark}"),
            None => println!("  watermark: (unresolved — anchor not found in source)"),
        }
        if let Some(confidence) = report.confidence {
            println!("  confidence: {confidence}");
        }
        if let Some(relates) = &report.relates_to {
            println!("  relates_to: {relates}");
        }
        println!("  status: {}", report.status);
        println!("  stored in {}", report.db);
    }

    Ok(())
}

#[derive(Serialize)]
struct FindingListItem {
    id: String,
    kind: String,
    title: String,
    body: String,
    status: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    confidence: Option<f64>,
    about: String,
    anchor_node: String,
    anchor_kind: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    captured_watermark: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    current_watermark: Option<String>,
    stale: bool,
    relates_to: Vec<String>,
}

#[derive(Serialize)]
struct FindingListReport {
    db: String,
    total: usize,
    findings: Vec<FindingListItem>,
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn cmd_finding_list(
    path: &Path,
    about: Option<&str>,
    kind: Option<&str>,
    status: Option<&str>,
    include_stale: bool,
    scope: Option<&str>,
    json_output: bool,
    pretty: bool,
) -> Result<()> {
    let root = lint::resolve_project_root_or_canonical_path(path)?;
    let db_path = findings_db_path(&root);

    // Fail closed gracefully: an absent findings.db is an empty result, never
    // a crash. The store is created lazily on the first `add`.
    if !db_path.exists() {
        return emit_list(&db_path, Vec::new(), json_output, pretty);
    }

    let store = SqliteGraphStore::open_read_only_resilient(&db_path)?;

    let mut items = Vec::new();
    for finding_kind in ["finding", "decision", "note"] {
        if let Some(filter) = kind
            && filter != finding_kind
        {
            continue;
        }
        for node in store.nodes_by_kind(finding_kind)? {
            let node_about = node.properties.get("about").cloned().unwrap_or_default();
            if let Some(filter) = about
                && filter != node_about
            {
                continue;
            }
            let node_status = node
                .properties
                .get("status")
                .cloned()
                .unwrap_or_else(|| "trusted".to_string());
            if let Some(filter) = status
                && filter != node_status
            {
                continue;
            }

            let captured = node.properties.get("watermark").cloned();
            let anchor_kind = node
                .properties
                .get("anchor_kind")
                .cloned()
                .unwrap_or_else(|| "file".to_string());

            // Re-resolve the anchor's current content hash to compute staleness.
            let current = if captured.is_some() {
                match resolve_anchor(&root, &node_about, scope) {
                    Ok(anchor) => anchor.watermark,
                    Err(_) => None,
                }
            } else {
                None
            };
            // Stale when both watermarks resolve and differ. When the captured
            // watermark exists but the anchor can no longer be resolved, treat
            // it as stale too (the code moved or was deleted).
            let stale = match (&captured, &current) {
                (Some(captured), Some(current)) => captured != current,
                (Some(_), None) => true,
                _ => false,
            };

            if stale && !include_stale {
                continue;
            }

            let relates_to = store
                .outgoing_edges(&node.id, Some("relates_to"))?
                .into_iter()
                .map(|edge| edge.to_id)
                .collect();

            items.push(FindingListItem {
                id: node.id.clone(),
                kind: finding_kind.to_string(),
                title: node.properties.get("title").cloned().unwrap_or_default(),
                body: node.properties.get("body").cloned().unwrap_or_default(),
                status: node_status,
                confidence: node
                    .properties
                    .get("confidence")
                    .and_then(|value| value.parse::<f64>().ok()),
                about: node_about,
                anchor_node: node.properties.get("anchor_node").cloned().unwrap_or_default(),
                anchor_kind,
                captured_watermark: captured,
                current_watermark: current,
                stale,
                relates_to,
            });
        }
    }

    items.sort_by(|left, right| left.id.cmp(&right.id));
    emit_list(&db_path, items, json_output, pretty)
}

/// A trusted, fresh finding selected for hot-path injection (#trt1p2). This is
/// the structured shape consumed by `context-pack` (and, later, `search` /
/// `explain`) so authored "why" rides the same envelope as the code result set.
#[derive(Clone, Serialize)]
pub(crate) struct InjectableFinding {
    pub(crate) id: String,
    pub(crate) kind: String,
    pub(crate) title: String,
    pub(crate) body: String,
    pub(crate) about: String,
    pub(crate) anchor_kind: String,
    pub(crate) confidence: Option<f64>,
}

/// Collect trusted, non-stale findings whose `about` anchor is one of
/// `about_keys`, for hot-path injection (#trt1p2). Only `trusted` + fresh
/// findings are returned: `draft` findings and findings whose anchor has
/// advanced past the captured watermark are excluded, so confident-but-wrong or
/// unverified authored context never rides the agent hot path. This is the
/// inverse of the failure mode the layer is designed against (a graph filling
/// with stale, confident findings). Fails open to an empty vec — never an error
/// — when the dedicated `findings.db` is absent, so callers can inject
/// unconditionally without a pre-check.
pub(crate) fn collect_injectable_findings(
    root: &Path,
    about_keys: &std::collections::BTreeSet<String>,
    scope: Option<&str>,
) -> Result<Vec<InjectableFinding>> {
    if about_keys.is_empty() {
        return Ok(Vec::new());
    }
    let db_path = findings_db_path(root);
    if !db_path.exists() {
        return Ok(Vec::new());
    }
    let store = SqliteGraphStore::open_read_only_resilient(&db_path)?;

    let mut items = Vec::new();
    for finding_kind in ["finding", "decision", "note"] {
        for node in store.nodes_by_kind(finding_kind)? {
            let about = node.properties.get("about").cloned().unwrap_or_default();
            if !about_keys.contains(&about) {
                continue;
            }
            // Trusted only: draft findings (including passive-harvest drafts)
            // are excluded from default injection.
            let status = node
                .properties
                .get("status")
                .cloned()
                .unwrap_or_else(|| "trusted".to_string());
            if status != "trusted" {
                continue;
            }
            // Fresh only: re-resolve the anchor's current content hash and skip
            // the finding when it differs from the captured watermark or the
            // anchor can no longer be resolved. A finding with no captured
            // watermark could not be freshness-verified at capture and is not
            // injected.
            let Some(captured) = node.properties.get("watermark").cloned() else {
                continue;
            };
            let current = resolve_anchor(root, &about, scope)
                .ok()
                .and_then(|anchor| anchor.watermark);
            if current.as_deref() != Some(captured.as_str()) {
                continue;
            }

            let anchor_kind = node
                .properties
                .get("anchor_kind")
                .cloned()
                .unwrap_or_else(|| "file".to_string());
            items.push(InjectableFinding {
                id: node.id.clone(),
                kind: finding_kind.to_string(),
                title: node.properties.get("title").cloned().unwrap_or_default(),
                body: node.properties.get("body").cloned().unwrap_or_default(),
                about,
                anchor_kind,
                confidence: node
                    .properties
                    .get("confidence")
                    .and_then(|value| value.parse::<f64>().ok()),
            });
        }
    }

    items.sort_by(|left, right| left.id.cmp(&right.id));
    Ok(items)
}

/// A trusted, fresh finding folded into `search` / `explain` output because it
/// concerns a node in the result set (#trt1p2b). Carries an `expand` command
/// back to the full anchored set, mirroring the context-pack preview shape.
#[derive(Clone, Serialize)]
pub(crate) struct ResultSetFindingPreview {
    pub(crate) id: String,
    pub(crate) kind: String,
    pub(crate) title: String,
    pub(crate) about: String,
    pub(crate) anchor_kind: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) confidence: Option<f64>,
    pub(crate) body: String,
    pub(crate) expand: String,
}

fn truncate_finding_body(body: &str, max_bytes: usize) -> String {
    if body.len() <= max_bytes {
        return body.to_string();
    }
    let mut end = max_bytes;
    while end > 0 && !body.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", &body[..end])
}

/// Collect trusted, fresh findings for nodes in `about_keys` as previews for
/// `search` / `explain` hot-path injection (#trt1p2b). Reuses the Phase 2
/// trusted+fresh contract, caps the count, and truncates bodies. Fails open to
/// an empty vec.
pub(crate) fn collect_result_set_finding_previews(
    root: &Path,
    about_keys: &BTreeSet<String>,
    scope: Option<&str>,
    max_items: usize,
    max_body_bytes: usize,
) -> Vec<ResultSetFindingPreview> {
    collect_injectable_findings(root, about_keys, scope)
        .unwrap_or_default()
        .into_iter()
        .take(max_items)
        .map(|finding| ResultSetFindingPreview {
            expand: format!("tsift finding list --about '{}' --json", finding.about),
            id: finding.id,
            kind: finding.kind,
            title: finding.title,
            anchor_kind: finding.anchor_kind,
            confidence: finding.confidence,
            body: truncate_finding_body(&finding.body, max_body_bytes),
            about: finding.about,
        })
        .collect()
}

fn emit_list(
    db_path: &Path,
    items: Vec<FindingListItem>,
    json_output: bool,
    pretty: bool,
) -> Result<()> {
    if json_output {
        let report = FindingListReport {
            db: db_path.display().to_string(),
            total: items.len(),
            findings: items,
        };
        let rendered = if pretty {
            serde_json::to_string_pretty(&report)?
        } else {
            serde_json::to_string(&report)?
        };
        println!("{rendered}");
    } else if items.is_empty() {
        println!("no findings ({})", db_path.display());
    } else {
        println!("findings[{}]:", items.len());
        for item in &items {
            let stale = if item.stale { " STALE" } else { "" };
            println!("  {} [{}{}] {}", item.id, item.kind, stale, item.title);
            println!("    about: {} ({})", item.about, item.anchor_kind);
            println!("    status: {}", item.status);
            if let Some(confidence) = item.confidence {
                println!("    confidence: {confidence}");
            }
            if !item.relates_to.is_empty() {
                println!("    relates_to: {}", item.relates_to.join(", "));
            }
        }
    }
    Ok(())
}

// --- Phase 4 (#trt1p4): config-gated passive harvest + draft→trusted promotion -

/// Decision/insight signal phrases. A line is a harvest candidate only when it
/// pairs one of these with a code token that resolves to an indexed symbol or
/// real file, so we capture authored "why" rather than arbitrary prose. Harvest
/// is deliberately generous (everything lands as `draft`, excluded from
/// injection until promoted) — precision is the promotion gate's job, not the
/// extractor's.
const HARVEST_SIGNALS: &[&str] = &[
    "decided",
    "decision",
    "invariant",
    "gotcha",
    "by design",
    "intentional",
    "must not",
    "must always",
    "the reason",
    "durability",
    "fail-closed",
    "fails open",
    "fail closed",
    "source of truth",
    "never overwrite",
];

fn harvest_kind_for(line_lower: &str) -> &'static str {
    if line_lower.contains("decid")
        || line_lower.contains("decision")
        || line_lower.contains("by design")
        || line_lower.contains("intentional")
    {
        "decision"
    } else {
        "note"
    }
}

/// Inline-code (`` `token` ``) spans of a line, in order. Standard markdown
/// inline-code parse: odd-indexed backtick segments are code.
fn inline_code_tokens(line: &str) -> Vec<String> {
    line.split('`')
        .enumerate()
        .filter_map(|(index, segment)| {
            let trimmed = segment.trim();
            (index % 2 == 1 && !trimmed.is_empty()).then(|| trimmed.to_string())
        })
        .collect()
}

/// Drop a leading YAML frontmatter block (`---` … `---`) so harvest scans the
/// authored body, not archive metadata.
fn strip_frontmatter(text: &str) -> &str {
    let Some(rest) = text.strip_prefix("---\n") else {
        return text;
    };
    match rest.find("\n---\n") {
        Some(end) => &rest[end + 5..],
        None => text,
    }
}

/// Strip leading markdown list/heading/quote markers and surrounding whitespace.
fn clean_harvest_line(line: &str) -> String {
    line.trim()
        .trim_start_matches(['#', '-', '*', '>', ' '])
        .trim()
        .to_string()
}

/// A concise title from a cleaned line: the first sentence, capped to ~100 chars
/// on a word boundary.
fn harvest_title(cleaned: &str) -> String {
    let first = cleaned
        .split_once(". ")
        .map(|(head, _)| head)
        .unwrap_or(cleaned)
        .trim();
    if first.chars().count() <= 100 {
        return first.to_string();
    }
    let mut out = String::new();
    for word in first.split_whitespace() {
        if out.chars().count() + word.chars().count() + 1 > 97 {
            break;
        }
        if !out.is_empty() {
            out.push(' ');
        }
        out.push_str(word);
    }
    out.push('…');
    out
}

#[derive(Serialize)]
struct HarvestedFinding {
    id: String,
    kind: String,
    title: String,
    about: String,
    anchor_kind: String,
    source: String,
}

#[derive(Serialize)]
struct FindingHarvestReport {
    db: String,
    enabled: bool,
    archives_scanned: usize,
    candidates: usize,
    inserted: usize,
    skipped_existing: usize,
    findings: Vec<HarvestedFinding>,
}

/// Upsert a passively-harvested `draft` finding: anchor node, finding node, and
/// `concerns` edge — mirroring `cmd_finding_add` but stamped as a draft from the
/// passive-harvest author with the source archive as provenance.
#[allow(clippy::too_many_arguments)]
fn upsert_harvested_finding(
    store: &SqliteGraphStore,
    id: &str,
    kind: &str,
    title: &str,
    body: &str,
    about: &str,
    anchor: &ResolvedAnchor,
    source: &str,
    observed: i64,
) -> Result<()> {
    let mut anchor_node =
        SubstrateGraphNode::new(anchor.node_id.clone(), anchor.node_kind, anchor.label.clone());
    for (key, value) in &anchor.properties {
        anchor_node = anchor_node.with_property(key.clone(), value.clone());
    }
    store.upsert_node(&anchor_node)?;

    let mut node = SubstrateGraphNode::new(id.to_string(), kind, title.to_string())
        .with_property("title", title.to_string())
        .with_property("body", body.to_string())
        .with_property("status", "draft")
        .with_property("author", "passive-harvest")
        .with_property("about", about.to_string())
        .with_property("anchor_node", anchor.node_id.clone())
        .with_property("anchor_kind", anchor.node_kind)
        .with_property("source", source.to_string());
    if let Some(watermark) = &anchor.watermark {
        node = node.with_property("watermark", watermark.clone());
    }
    let mut provenance =
        GraphProvenance::new("findings", format!("passive-harvest:{source}"));
    if let Some(watermark) = &anchor.watermark {
        provenance = provenance.with_content_hash(watermark.clone());
    }
    node = node.with_provenance(provenance);
    node = node.with_freshness(GraphFreshness {
        content_hash: anchor.watermark.clone(),
        observed_at_unix: Some(observed),
    });
    store.upsert_node(&node)?;

    let mut concerns = SubstrateGraphEdge::new(id.to_string(), anchor.node_id.clone(), "concerns");
    if let Some(watermark) = &anchor.watermark {
        concerns = concerns
            .with_property("watermark", watermark.clone())
            .with_freshness(GraphFreshness {
                content_hash: Some(watermark.clone()),
                observed_at_unix: Some(observed),
            });
    }
    store.upsert_edge(&concerns)?;
    Ok(())
}

/// Bound a single harvest run so a large archive set cannot create an unbounded
/// number of draft nodes in one pass.
const HARVEST_CAP: usize = 100;

pub(crate) fn cmd_finding_harvest(
    path: &Path,
    scope: Option<&str>,
    json_output: bool,
    pretty: bool,
) -> Result<()> {
    let root = lint::resolve_project_root_or_canonical_path(path)?;

    // Fail-closed config gate: the whole auto-capture path is off until the
    // user explicitly opts in.
    let config = IndexConfig::load(&root)?;
    if !config.findings.passive_harvest {
        bail!(
            "passive harvest is disabled (fail-closed). Enable it by adding to {}:\n\n[findings]\npassive_harvest = true",
            root.join(".tsift/config.toml").display()
        );
    }

    let archives_dir = root.join(".agent-doc/archives");
    let mut archive_files: Vec<PathBuf> = Vec::new();
    if let Ok(entries) = std::fs::read_dir(&archives_dir) {
        for entry in entries.flatten() {
            let candidate = entry.path();
            if candidate.extension().and_then(|ext| ext.to_str()) == Some("md") {
                archive_files.push(candidate);
            }
        }
    }
    archive_files.sort();

    let db_path = findings_db_path(&root);
    let store = SqliteGraphStore::open(&db_path)?;
    let observed = now_unix();

    let mut report = FindingHarvestReport {
        db: db_path.display().to_string(),
        enabled: true,
        archives_scanned: archive_files.len(),
        candidates: 0,
        inserted: 0,
        skipped_existing: 0,
        findings: Vec::new(),
    };
    let mut seen_ids: BTreeSet<String> = BTreeSet::new();

    'outer: for archive in &archive_files {
        let Ok(text) = std::fs::read_to_string(archive) else {
            continue;
        };
        let source = archive
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or_default()
            .to_string();
        for line in strip_frontmatter(&text).lines() {
            if report.inserted >= HARVEST_CAP {
                break 'outer;
            }
            let cleaned = clean_harvest_line(line);
            if cleaned.is_empty() {
                continue;
            }
            let lower = cleaned.to_ascii_lowercase();
            if !HARVEST_SIGNALS.iter().any(|signal| lower.contains(signal)) {
                continue;
            }
            // First inline-code token that resolves to a real anchor (indexed
            // symbol or readable file — a watermark proves resolution).
            let resolved = inline_code_tokens(line).into_iter().find_map(|token| {
                let anchor = resolve_anchor(&root, &token, scope).ok()?;
                anchor.watermark.as_ref()?;
                Some((token, anchor))
            });
            let Some((about, anchor)) = resolved else {
                continue;
            };

            report.candidates += 1;
            let kind = harvest_kind_for(&lower);
            let title = harvest_title(&cleaned);
            let id = finding_id(kind, &title, &about);
            if !seen_ids.insert(id.clone()) {
                continue;
            }
            // Never overwrite an existing finding (e.g. a trusted one) with a
            // re-harvested draft.
            if store.node(&id)?.is_some() {
                report.skipped_existing += 1;
                continue;
            }
            upsert_harvested_finding(
                &store, &id, kind, &title, &cleaned, &about, &anchor, &source, observed,
            )?;
            report.inserted += 1;
            report.findings.push(HarvestedFinding {
                id,
                kind: kind.to_string(),
                title,
                about,
                anchor_kind: anchor.node_kind.to_string(),
                source: source.clone(),
            });
        }
    }

    if json_output {
        let rendered = if pretty {
            serde_json::to_string_pretty(&report)?
        } else {
            serde_json::to_string(&report)?
        };
        println!("{rendered}");
    } else {
        println!(
            "harvested {} draft finding(s) from {} archive(s) ({} candidate(s), {} already present)",
            report.inserted, report.archives_scanned, report.candidates, report.skipped_existing
        );
        for finding in &report.findings {
            println!("  {} [{}] {}", finding.id, finding.kind, finding.title);
            println!("    about: {} · source: {}", finding.about, finding.source);
        }
        println!("  stored in {} (status: draft — promote with `tsift finding promote <id>`)", report.db);
    }
    Ok(())
}

#[derive(Serialize)]
struct FindingPromoteReport {
    id: String,
    kind: String,
    title: String,
    about: String,
    from_status: String,
    to_status: String,
    changed: bool,
    db: String,
}

pub(crate) fn cmd_finding_promote(
    path: &Path,
    id: &str,
    json_output: bool,
    pretty: bool,
) -> Result<()> {
    let root = lint::resolve_project_root_or_canonical_path(path)?;
    let db_path = findings_db_path(&root);
    if !db_path.exists() {
        bail!("no findings store at {}", db_path.display());
    }
    let store = SqliteGraphStore::open(&db_path)?;
    let Some(node) = store.node(id)? else {
        bail!("finding not found in {}: {id}", db_path.display());
    };

    let from_status = node
        .properties
        .get("status")
        .cloned()
        .unwrap_or_else(|| "trusted".to_string());
    let kind = node.kind.clone();
    let title = node.properties.get("title").cloned().unwrap_or_default();
    let about = node.properties.get("about").cloned().unwrap_or_default();

    let changed = from_status != "trusted";
    if changed {
        // with_property overwrites the status key; the rest of the node
        // (anchor, watermark, provenance, freshness) is preserved.
        let promoted = node.with_property("status", "trusted");
        store.upsert_node(&promoted)?;
    }

    let report = FindingPromoteReport {
        id: id.to_string(),
        kind,
        title,
        about,
        from_status,
        to_status: "trusted".to_string(),
        changed,
        db: db_path.display().to_string(),
    };

    if json_output {
        let rendered = if pretty {
            serde_json::to_string_pretty(&report)?
        } else {
            serde_json::to_string(&report)?
        };
        println!("{rendered}");
    } else if report.changed {
        println!("promoted {} [{}] {} → trusted", report.id, report.kind, report.title);
    } else {
        println!("{} is already trusted (no change)", report.id);
    }
    Ok(())
}
