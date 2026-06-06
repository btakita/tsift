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

use std::path::Path;

use anyhow::{Result, bail};
use serde::Serialize;
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
