//! File-backed decision manifests.
//!
//! Markdown is the reviewable authority. `decision import` validates the
//! manifest, then projects stable decision nodes into `.tsift/findings.db` so
//! existing finding retrieval can discover accepted decisions.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::path::{Component, Path};
use std::str::FromStr;

use anyhow::{Context, Result, bail};
use serde::Serialize;
use tsift_quality::lint;
use tsift_sqlite::{
    GraphEdge, GraphFreshness, GraphNode, GraphProvenance, GraphStore, SqliteGraphStore,
};

use super::finding::{ResolvedAnchor, findings_db_path, now_unix, resolve_anchor};

const MANIFEST_COLUMNS: [&str; 8] = [
    "id",
    "status",
    "date",
    "decision",
    "source",
    "anchors",
    "supersedes",
    "related",
];

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(transparent)]
struct DecisionId(String);

impl DecisionId {
    fn graph_id(&self) -> String {
        format!("decision:{}", self.0)
    }
}

impl fmt::Display for DecisionId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(formatter)
    }
}

impl FromStr for DecisionId {
    type Err = anyhow::Error;

    fn from_str(raw: &str) -> Result<Self> {
        let value = raw.trim();
        let suffix = value
            .strip_prefix("dec-")
            .ok_or_else(|| anyhow::anyhow!("decision id must start with `dec-`: `{value}`"))?;
        if suffix.is_empty()
            || suffix.starts_with('-')
            || suffix.ends_with('-')
            || suffix.contains("--")
            || !suffix
                .chars()
                .all(|ch| ch.is_ascii_lowercase() || ch.is_ascii_digit() || ch == '-')
        {
            bail!(
                "invalid decision id `{value}`; expected `dec-` followed by lowercase ASCII letters, digits, or single hyphens"
            );
        }
        Ok(Self(value.to_string()))
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "lowercase")]
enum DecisionStatus {
    Proposed,
    Accepted,
    Superseded,
    Rejected,
}

impl DecisionStatus {
    fn as_str(self) -> &'static str {
        match self {
            Self::Proposed => "proposed",
            Self::Accepted => "accepted",
            Self::Superseded => "superseded",
            Self::Rejected => "rejected",
        }
    }

    fn trust_status(self) -> &'static str {
        if self == Self::Accepted {
            "trusted"
        } else {
            "draft"
        }
    }
}

impl FromStr for DecisionStatus {
    type Err = anyhow::Error;

    fn from_str(raw: &str) -> Result<Self> {
        match raw.trim() {
            "proposed" => Ok(Self::Proposed),
            "accepted" => Ok(Self::Accepted),
            "superseded" => Ok(Self::Superseded),
            "rejected" => Ok(Self::Rejected),
            other => bail!(
                "unsupported decision status `{other}`; expected proposed|accepted|superseded|rejected"
            ),
        }
    }
}

#[derive(Clone, Debug, Serialize)]
struct DecisionRecord {
    id: DecisionId,
    status: DecisionStatus,
    date: String,
    decision: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    source: Option<String>,
    anchors: Vec<String>,
    supersedes: Vec<DecisionId>,
    related: Vec<DecisionId>,
    line: usize,
}

#[derive(Serialize)]
struct DecisionLintReport {
    valid: bool,
    manifest: String,
    decision_count: usize,
    accepted_count: usize,
    decisions: Vec<DecisionRecord>,
}

#[derive(Serialize)]
struct DecisionImportReport {
    manifest: String,
    manifest_hash: String,
    db: String,
    decisions_imported: usize,
    accepted_decisions: usize,
    nodes_upserted: usize,
    edges_upserted: usize,
}

struct ParsedManifest {
    relative_path: String,
    content_hash: String,
    decisions: Vec<DecisionRecord>,
}

pub(crate) fn cmd_decision_lint(
    path: &Path,
    manifest: &Path,
    json_output: bool,
    pretty: bool,
) -> Result<()> {
    let root = lint::resolve_project_root_or_canonical_path(path)?;
    let parsed = load_manifest(&root, manifest)?;
    let report = DecisionLintReport {
        valid: true,
        manifest: parsed.relative_path,
        decision_count: parsed.decisions.len(),
        accepted_count: parsed
            .decisions
            .iter()
            .filter(|decision| decision.status == DecisionStatus::Accepted)
            .count(),
        decisions: parsed.decisions,
    };

    if json_output {
        emit_json(&report, pretty)?;
    } else {
        println!(
            "decision manifest OK: {} ({} decisions, {} accepted)",
            report.manifest, report.decision_count, report.accepted_count
        );
    }
    Ok(())
}

pub(crate) fn cmd_decision_import(
    path: &Path,
    manifest: &Path,
    scope: Option<&str>,
    json_output: bool,
    pretty: bool,
) -> Result<()> {
    let root = lint::resolve_project_root_or_canonical_path(path)?;
    let parsed = load_manifest(&root, manifest)?;
    let db_path = findings_db_path(&root);
    let store = SqliteGraphStore::open(&db_path)?;
    let observed = now_unix();
    let mut nodes_upserted = 0;
    let mut edges_upserted = 0;

    // All decision nodes must exist before relationship edges are inserted.
    for decision in &parsed.decisions {
        let primary_about = decision
            .anchors
            .first()
            .map(|anchor| anchor_target(anchor))
            .or_else(|| decision.source.as_deref().and_then(local_source_path))
            .unwrap_or(parsed.relative_path.as_str());
        let primary = match decision.anchors.first() {
            Some(anchor) => resolve_manifest_anchor(&root, anchor, scope)?,
            None => resolve_anchor(&root, primary_about, scope)?,
        };
        upsert_anchor(&store, &primary)?;
        nodes_upserted += 1;

        let mut node = GraphNode::new(
            decision.id.graph_id(),
            "decision",
            decision.decision.clone(),
        )
        .with_property("title", decision.decision.clone())
        .with_property("body", decision.decision.clone())
        .with_property("decision_id", decision.id.to_string())
        .with_property("decision_status", decision.status.as_str())
        .with_property("status", decision.status.trust_status())
        .with_property("date", decision.date.clone())
        .with_property("author", "manifest")
        .with_property("manifest", parsed.relative_path.clone())
        .with_property("manifest_hash", parsed.content_hash.clone())
        .with_property("about", primary_about)
        .with_property("anchor_node", primary.node_id.clone())
        .with_property("anchor_kind", primary.node_kind);
        if let Some(source) = &decision.source {
            node = node.with_property("source", source.clone());
        }
        if let Some(watermark) = &primary.watermark {
            node = node.with_property("watermark", watermark.clone());
        }
        node = node
            .with_provenance(
                GraphProvenance::new(
                    "decision-manifest",
                    format!("{}#{}", parsed.relative_path, decision.id),
                )
                .with_content_hash(parsed.content_hash.clone()),
            )
            .with_freshness(GraphFreshness {
                content_hash: primary.watermark.clone(),
                observed_at_unix: Some(observed),
            });
        store.upsert_node(&node)?;
        nodes_upserted += 1;
    }

    for decision in &parsed.decisions {
        let decision_node = decision.id.graph_id();
        let primary_about = decision
            .anchors
            .first()
            .map(|anchor| anchor_target(anchor))
            .or_else(|| decision.source.as_deref().and_then(local_source_path))
            .unwrap_or(parsed.relative_path.as_str());
        let mut anchors = decision.anchors.clone();
        if anchors.is_empty() {
            anchors.push(format!("file:{primary_about}"));
        }
        for anchor in &anchors {
            let resolved = resolve_manifest_anchor(&root, anchor, scope)?;
            upsert_anchor(&store, &resolved)?;
            nodes_upserted += 1;
            let mut edge =
                GraphEdge::new(decision_node.clone(), resolved.node_id.clone(), "concerns");
            if let Some(watermark) = &resolved.watermark {
                edge = edge
                    .with_property("watermark", watermark.clone())
                    .with_freshness(GraphFreshness {
                        content_hash: Some(watermark.clone()),
                        observed_at_unix: Some(observed),
                    });
            }
            store.upsert_edge(&edge)?;
            edges_upserted += 1;
        }

        if let Some(source) = &decision.source {
            let source_node = source_node(source);
            store.upsert_node(&source_node)?;
            nodes_upserted += 1;
            store.upsert_edge(&GraphEdge::new(
                decision_node.clone(),
                source_node.id,
                "documents",
            ))?;
            edges_upserted += 1;
        }

        for target in &decision.supersedes {
            store.upsert_edge(&GraphEdge::new(
                decision_node.clone(),
                target.graph_id(),
                "supersedes",
            ))?;
            edges_upserted += 1;
        }
        for target in &decision.related {
            store.upsert_edge(&GraphEdge::new(
                decision_node.clone(),
                target.graph_id(),
                "relates_to",
            ))?;
            edges_upserted += 1;
        }
    }

    let report = DecisionImportReport {
        manifest: parsed.relative_path,
        manifest_hash: parsed.content_hash,
        db: db_path.display().to_string(),
        decisions_imported: parsed.decisions.len(),
        accepted_decisions: parsed
            .decisions
            .iter()
            .filter(|decision| decision.status == DecisionStatus::Accepted)
            .count(),
        nodes_upserted,
        edges_upserted,
    };
    if json_output {
        emit_json(&report, pretty)?;
    } else {
        println!(
            "imported {} decisions ({} accepted) from {}",
            report.decisions_imported, report.accepted_decisions, report.manifest
        );
        println!("  stored in {}", report.db);
        println!(
            "  upserted {} nodes and {} edges",
            report.nodes_upserted, report.edges_upserted
        );
    }
    Ok(())
}

fn load_manifest(root: &Path, manifest: &Path) -> Result<ParsedManifest> {
    if manifest.is_absolute() {
        bail!(
            "decision manifest must be repository-relative: {}",
            manifest.display()
        );
    }
    validate_relative_path(manifest, "decision manifest")?;
    let path = root.join(manifest);
    let canonical = path
        .canonicalize()
        .with_context(|| format!("reading decision manifest {}", path.display()))?;
    let canonical_root = root.canonicalize()?;
    let relative_path = canonical
        .strip_prefix(&canonical_root)
        .with_context(|| {
            format!(
                "decision manifest escapes project root: {}",
                manifest.display()
            )
        })?
        .to_string_lossy()
        .replace('\\', "/");
    let content = std::fs::read_to_string(&canonical)
        .with_context(|| format!("reading decision manifest {}", canonical.display()))?;
    let decisions = parse_manifest(root, &content)?;
    validate_relationships(&decisions)?;
    Ok(ParsedManifest {
        relative_path,
        content_hash: tsift_graph::source_content_hash(content.as_bytes()),
        decisions,
    })
}

fn parse_manifest(root: &Path, content: &str) -> Result<Vec<DecisionRecord>> {
    let lines: Vec<&str> = content.lines().collect();
    let mut header_index = None;
    for (index, line) in lines.iter().enumerate() {
        let cells = split_markdown_row(line);
        if cells
            .iter()
            .map(|cell| cell.to_ascii_lowercase())
            .collect::<Vec<_>>()
            == MANIFEST_COLUMNS
        {
            header_index = Some(index);
            break;
        }
    }
    let header_index = header_index.ok_or_else(|| {
        anyhow::anyhow!(
            "decision manifest must contain a Markdown table with columns: {}",
            MANIFEST_COLUMNS.join(" | ")
        )
    })?;
    let separator = lines
        .get(header_index + 1)
        .ok_or_else(|| anyhow::anyhow!("decision manifest table is missing its separator row"))?;
    let separator_cells = split_markdown_row(separator);
    if separator_cells.len() != MANIFEST_COLUMNS.len()
        || !separator_cells.iter().all(|cell| {
            let value = cell.trim().trim_matches(':');
            value.len() >= 3 && value.chars().all(|ch| ch == '-')
        })
    {
        bail!("decision manifest table has an invalid separator row");
    }

    let mut decisions = Vec::new();
    let mut ids = BTreeSet::new();
    for (index, line) in lines.iter().enumerate().skip(header_index + 2) {
        if line.trim().is_empty() {
            if !decisions.is_empty() {
                break;
            }
            continue;
        }
        if !line.trim_start().starts_with('|') {
            if !decisions.is_empty() {
                break;
            }
            continue;
        }
        let cells = split_markdown_row(line);
        let line_number = index + 1;
        if cells.len() != MANIFEST_COLUMNS.len() {
            bail!(
                "decision manifest line {line_number} has {} columns; expected {}",
                cells.len(),
                MANIFEST_COLUMNS.len()
            );
        }
        let id = DecisionId::from_str(&cells[0])
            .with_context(|| format!("decision manifest line {line_number}"))?;
        if !ids.insert(id.clone()) {
            bail!("duplicate decision id `{id}` at line {line_number}");
        }
        let status = DecisionStatus::from_str(&cells[1])
            .with_context(|| format!("decision `{id}` at line {line_number}"))?;
        validate_date(&cells[2])
            .with_context(|| format!("decision `{id}` at line {line_number}"))?;
        let decision = cells[3].trim().to_string();
        if decision.is_empty() || decision == "-" {
            bail!("decision `{id}` at line {line_number} has an empty decision summary");
        }
        let source = optional_cell(&cells[4]);
        if let Some(source) = &source {
            validate_source(root, source)
                .with_context(|| format!("decision `{id}` at line {line_number}"))?;
        }
        let anchors = list_cell(&cells[5]);
        for anchor in &anchors {
            validate_anchor(root, anchor)
                .with_context(|| format!("decision `{id}` at line {line_number}"))?;
        }
        let supersedes = parse_id_list(&cells[6], &id, "supersedes", line_number)?;
        let related = parse_id_list(&cells[7], &id, "related", line_number)?;
        decisions.push(DecisionRecord {
            id,
            status,
            date: cells[2].trim().to_string(),
            decision,
            source,
            anchors,
            supersedes,
            related,
            line: line_number,
        });
    }
    if decisions.is_empty() {
        bail!("decision manifest table contains no decisions");
    }
    Ok(decisions)
}

fn validate_relationships(decisions: &[DecisionRecord]) -> Result<()> {
    let by_id: BTreeMap<&DecisionId, &DecisionRecord> = decisions
        .iter()
        .map(|decision| (&decision.id, decision))
        .collect();
    let mut superseded_by: BTreeMap<&DecisionId, Vec<&DecisionId>> = BTreeMap::new();
    for decision in decisions {
        if matches!(
            decision.status,
            DecisionStatus::Proposed | DecisionStatus::Rejected
        ) && !decision.supersedes.is_empty()
        {
            bail!(
                "decision `{}` is {} and cannot supersede another decision",
                decision.id,
                decision.status.as_str()
            );
        }
        for target in decision.supersedes.iter().chain(&decision.related) {
            if !by_id.contains_key(target) {
                bail!(
                    "decision `{}` references missing decision `{target}`",
                    decision.id
                );
            }
        }
        for target in &decision.supersedes {
            let target_record = by_id[target];
            if target_record.status != DecisionStatus::Superseded {
                bail!(
                    "decision `{}` supersedes `{target}`, but the target status is `{}` instead of `superseded`",
                    decision.id,
                    target_record.status.as_str()
                );
            }
            superseded_by.entry(target).or_default().push(&decision.id);
        }
    }
    for decision in decisions {
        if decision.status == DecisionStatus::Superseded {
            match superseded_by.get(&decision.id).map(Vec::as_slice) {
                None | Some([]) => bail!(
                    "decision `{}` is superseded but no decision names it in `supersedes`",
                    decision.id
                ),
                Some([_, _, ..]) => bail!(
                    "decision `{}` is superseded by more than one decision",
                    decision.id
                ),
                Some([_]) => {}
            }
        }
    }
    validate_supersedes_acyclic(decisions)
}

fn validate_supersedes_acyclic(decisions: &[DecisionRecord]) -> Result<()> {
    let edges: BTreeMap<&DecisionId, &[DecisionId]> = decisions
        .iter()
        .map(|decision| (&decision.id, decision.supersedes.as_slice()))
        .collect();
    let mut visiting = BTreeSet::new();
    let mut visited = BTreeSet::new();
    for decision in decisions {
        visit_supersedes(&decision.id, &edges, &mut visiting, &mut visited)?;
    }
    Ok(())
}

fn visit_supersedes<'a>(
    id: &'a DecisionId,
    edges: &BTreeMap<&'a DecisionId, &'a [DecisionId]>,
    visiting: &mut BTreeSet<&'a DecisionId>,
    visited: &mut BTreeSet<&'a DecisionId>,
) -> Result<()> {
    if visited.contains(id) {
        return Ok(());
    }
    if !visiting.insert(id) {
        bail!("decision supersedes relationship contains a cycle at `{id}`");
    }
    if let Some(targets) = edges.get(id) {
        for target in *targets {
            visit_supersedes(target, edges, visiting, visited)?;
        }
    }
    visiting.remove(id);
    visited.insert(id);
    Ok(())
}

fn split_markdown_row(line: &str) -> Vec<String> {
    let trimmed = line.trim().trim_start_matches('|').trim_end_matches('|');
    let mut cells = Vec::new();
    let mut cell = String::new();
    let mut escaped = false;
    for ch in trimmed.chars() {
        if escaped {
            cell.push(ch);
            escaped = false;
        } else if ch == '\\' {
            escaped = true;
        } else if ch == '|' {
            cells.push(cell.trim().to_string());
            cell.clear();
        } else {
            cell.push(ch);
        }
    }
    if escaped {
        cell.push('\\');
    }
    cells.push(cell.trim().to_string());
    cells
}

fn optional_cell(raw: &str) -> Option<String> {
    let value = raw.trim();
    (!value.is_empty() && value != "-").then(|| value.to_string())
}

fn list_cell(raw: &str) -> Vec<String> {
    raw.replace("<br/>", ",")
        .replace("<br>", ",")
        .split(',')
        .map(str::trim)
        .filter(|value| !value.is_empty() && *value != "-")
        .map(ToOwned::to_owned)
        .collect()
}

fn parse_id_list(
    raw: &str,
    owner: &DecisionId,
    field: &str,
    line: usize,
) -> Result<Vec<DecisionId>> {
    let mut seen = BTreeSet::new();
    let mut ids = Vec::new();
    for value in list_cell(raw) {
        let id = DecisionId::from_str(&value)
            .with_context(|| format!("decision `{owner}` {field} at line {line}"))?;
        if &id == owner {
            bail!("decision `{owner}` cannot reference itself in `{field}`");
        }
        if !seen.insert(id.clone()) {
            bail!("decision `{owner}` repeats `{id}` in `{field}`");
        }
        ids.push(id);
    }
    Ok(ids)
}

fn validate_date(raw: &str) -> Result<()> {
    let parts: Vec<&str> = raw.trim().split('-').collect();
    if parts.len() != 3 || parts[0].len() != 4 || parts[1].len() != 2 || parts[2].len() != 2 {
        bail!("date must use YYYY-MM-DD (got `{}`)", raw.trim());
    }
    let year: u32 = parts[0].parse().context("date year is not numeric")?;
    let month: u32 = parts[1].parse().context("date month is not numeric")?;
    let day: u32 = parts[2].parse().context("date day is not numeric")?;
    let leap = year.is_multiple_of(4) && (!year.is_multiple_of(100) || year.is_multiple_of(400));
    let max_day = match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 if leap => 29,
        2 => 28,
        _ => bail!("date month is out of range: {month}"),
    };
    if day == 0 || day > max_day {
        bail!("date day is out of range: {day}");
    }
    Ok(())
}

fn validate_source(root: &Path, source: &str) -> Result<()> {
    if source.starts_with("https://") || source.starts_with("http://") {
        return Ok(());
    }
    let path = local_source_path(source).ok_or_else(|| anyhow::anyhow!("source path is empty"))?;
    let path = Path::new(path);
    validate_relative_path(path, "source")?;
    if !root.join(path).is_file() {
        bail!("source file does not exist: {source}");
    }
    Ok(())
}

fn validate_anchor(root: &Path, anchor: &str) -> Result<()> {
    if let Some(symbol) = anchor.strip_prefix("code_symbol:") {
        if symbol.trim().is_empty() {
            bail!("code_symbol anchor is empty");
        }
        return Ok(());
    }
    let path = anchor
        .strip_prefix("file:")
        .ok_or_else(|| anyhow::anyhow!("anchor must start with `code_symbol:` or `file:`"))?;
    let path = Path::new(path);
    validate_relative_path(path, "file anchor")?;
    if !root.join(path).is_file() {
        bail!("anchor file does not exist: {}", path.display());
    }
    Ok(())
}

fn validate_relative_path(path: &Path, label: &str) -> Result<()> {
    if path.as_os_str().is_empty() || path.is_absolute() {
        bail!("{label} must be a non-empty repository-relative path");
    }
    if path.components().any(|component| {
        matches!(
            component,
            Component::ParentDir | Component::RootDir | Component::Prefix(_)
        )
    }) {
        bail!("{label} must not escape the repository: {}", path.display());
    }
    Ok(())
}

fn anchor_target(anchor: &str) -> &str {
    anchor
        .strip_prefix("code_symbol:")
        .or_else(|| anchor.strip_prefix("file:"))
        .unwrap_or(anchor)
}

fn resolve_manifest_anchor(
    root: &Path,
    anchor: &str,
    scope: Option<&str>,
) -> Result<ResolvedAnchor> {
    let resolved = resolve_anchor(root, anchor_target(anchor), scope)?;
    if anchor.starts_with("code_symbol:") && resolved.node_kind != "code_symbol" {
        bail!(
            "code-symbol anchor `{anchor}` is not present in the current index; run `tsift index` and retry"
        );
    }
    if anchor.starts_with("file:") && resolved.node_kind != "file" {
        bail!("file anchor `{anchor}` resolved as a symbol instead of a file");
    }
    Ok(resolved)
}

fn local_source_path(source: &str) -> Option<&str> {
    if source.starts_with("https://") || source.starts_with("http://") {
        return None;
    }
    source.split('#').next().filter(|value| !value.is_empty())
}

fn source_node(source: &str) -> GraphNode {
    if let Some(path) = local_source_path(source) {
        GraphNode::new(format!("file:{path}"), "file", path)
            .with_property("path", path)
            .with_property("source", source)
    } else {
        let id = format!("document:{}", blake3::hash(source.as_bytes()).to_hex());
        GraphNode::new(id, "document", source).with_property("url", source)
    }
}

fn upsert_anchor(store: &SqliteGraphStore, anchor: &ResolvedAnchor) -> Result<()> {
    let mut node = GraphNode::new(
        anchor.node_id.clone(),
        anchor.node_kind,
        anchor.label.clone(),
    );
    for (key, value) in &anchor.properties {
        node = node.with_property(key.clone(), value.clone());
    }
    store.upsert_node(&node)
}

fn emit_json<T: Serialize>(value: &T, pretty: bool) -> Result<()> {
    let rendered = if pretty {
        serde_json::to_string_pretty(value)?
    } else {
        serde_json::to_string(value)?
    };
    println!("{rendered}");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decision_id_is_domain_specific_and_stable() {
        assert_eq!(
            DecisionId::from_str("dec-0007").unwrap().graph_id(),
            "decision:dec-0007"
        );
        assert!(DecisionId::from_str("work-0007").is_err());
        assert!(DecisionId::from_str("dec-Bad").is_err());
        assert!(DecisionId::from_str("dec-bad--id").is_err());
    }

    #[test]
    fn markdown_rows_preserve_escaped_pipes() {
        assert_eq!(
            split_markdown_row("| dec-1 | accepted | use A \\| B |"),
            vec!["dec-1", "accepted", "use A | B"]
        );
    }

    #[test]
    fn date_validation_checks_calendar_days() {
        assert!(validate_date("2024-02-29").is_ok());
        assert!(validate_date("2023-02-29").is_err());
        assert!(validate_date("2026-13-01").is_err());
    }
}
