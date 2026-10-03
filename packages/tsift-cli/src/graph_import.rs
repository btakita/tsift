//! External edge-file import for `graph-db` (`#sdktsiftedges`).
//!
//! `tsift graph-db import <FILE>` loads nodes and edges that some other tool
//! produced (a code generator's trace, a dependency manifest, a hand-written
//! map) into the same graph the indexer projects. The file is validated,
//! normalized into the generic `tsift-graph-edges/v1` shape, and stored under
//! the graph.db directory in `graph-imports/`. Every graph refresh then appends
//! the stored imports to the traversal projection, so an import survives
//! `graph-db refresh` and a re-import of the same `source` replaces that
//! source's rows instead of accumulating them.
//!
//! Endpoints join to indexed nodes when they can: a node with a `join` names an
//! indexed symbol (by name, optionally narrowed by language or path) or a
//! declaration site (`path` + `line`). A joined endpoint *is* the indexed node,
//! so a query that starts from an indexed contract symbol walks straight into
//! the imported edges. An endpoint that matches nothing becomes an imported
//! node of its own (kind `external` when the file did not declare it) and is
//! counted rather than rejected.
//!
//! haiven-sdk's `codegen/trace.json` is accepted natively through a small
//! adapter that lowers it into the generic shape (see [`lower_haiven_trace`]).

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};
use tsift_sqlite::{GraphEdge, GraphNode, GraphProvenance};

/// Format tag written into every normalized edge file.
pub(crate) const GRAPH_EDGES_FORMAT: &str = "tsift-graph-edges/v1";
/// Provider recorded on every imported node, edge, and source summary.
pub(crate) const GRAPH_IMPORT_PROVIDER: &str = "tsift-graph-import";
/// Kind of the one summary node each import source projects.
pub(crate) const GRAPH_IMPORT_SOURCE_KIND: &str = "graph_import";
/// Kind given to an endpoint the file never declared and nothing indexed matched.
pub(crate) const GRAPH_IMPORT_EXTERNAL_KIND: &str = "external";
/// A name join that matches more indexed nodes than this links none of them
/// and keeps the imported node instead; it is too ambiguous to be a join.
const MAX_JOIN_MATCHES: usize = 8;
/// Directory (beside graph.db) that holds normalized import files.
const GRAPH_IMPORTS_DIR: &str = "graph-imports";

/// The generic edge file: optional nodes, required edges.
#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq)]
pub(crate) struct EdgeFile {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub format: Option<String>,
    /// Stable id of this import. Re-importing the same source replaces it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
    /// Where the rows came from (the original file), for provenance only.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub imported_from: Option<String>,
    /// Which adapter produced the rows (`edges` for a native file).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub adapter: Option<String>,
    #[serde(default)]
    pub nodes: Vec<EdgeFileNode>,
    pub edges: Vec<EdgeFileEdge>,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq)]
pub(crate) struct EdgeFileNode {
    /// File-local id that edges refer to.
    pub id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kind: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub join: Option<JoinSpec>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub properties: BTreeMap<String, Value>,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
pub(crate) struct EdgeFileEdge {
    pub from: String,
    pub to: String,
    pub kind: String,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub properties: BTreeMap<String, Value>,
}

/// How a node finds its indexed counterpart. A bare string is a name join.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
#[serde(untagged)]
pub(crate) enum JoinSpec {
    Name(String),
    Spec(JoinFields),
}

#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq)]
pub(crate) struct JoinFields {
    /// Indexed symbol name (or qualified `Owner.member` name).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// Restrict a name join to symbols of these index languages (e.g. `json`).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub languages: Vec<String>,
    /// Restrict to symbols in this root-relative file; alone, joins the file node.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    /// With `path`: the 1-based line of the declaration.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub line: Option<i64>,
    /// Fallback names tried in order when `name` matches nothing (e.g. an
    /// operation known both as `GET /path` and by its `operationId`).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub aliases: Vec<String>,
}

impl JoinSpec {
    fn fields(&self) -> JoinFields {
        match self {
            JoinSpec::Name(name) => JoinFields {
                name: Some(name.clone()),
                ..JoinFields::default()
            },
            JoinSpec::Spec(fields) => fields.clone(),
        }
    }
}

/// Input format selector for `graph-db import --format`.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ImportFormat {
    Auto,
    Edges,
    HaivenTrace,
}

/// Per-source resolution counts, also stored on the source's summary node.
#[derive(Clone, Debug, Default, Serialize, PartialEq, Eq)]
pub(crate) struct GraphImportStats {
    pub source: String,
    pub adapter: String,
    pub declared_nodes: usize,
    pub edges: usize,
    /// Edges skipped because an identical (from, to, kind) row already exists.
    pub duplicate_edges: usize,
    /// Declared nodes whose `join` landed on indexed nodes.
    pub joined_nodes: usize,
    /// Declared nodes with a `join` that matched nothing (kept as imported nodes).
    pub unmatched_joins: usize,
    /// Joins that matched more than one indexed node (all are linked) or too
    /// many to be meaningful (none are linked).
    pub ambiguous_joins: usize,
    /// Undeclared edge endpoints that matched an indexed node by name.
    pub matched_endpoints: usize,
    /// Undeclared edge endpoints that matched nothing and became `external` nodes.
    pub external_endpoints: usize,
    /// A sample of unmatched join names / external endpoints, for the report.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub unmatched_sample: Vec<String>,
}

const UNMATCHED_SAMPLE_LIMIT: usize = 12;

/// Directory that holds normalized imports for the graph.db at `graph_db`.
pub(crate) fn graph_imports_dir_for_db(graph_db: &Path) -> PathBuf {
    graph_db
        .parent()
        .map(|parent| parent.join(GRAPH_IMPORTS_DIR))
        .unwrap_or_else(|| PathBuf::from(GRAPH_IMPORTS_DIR))
}

/// File name a source is stored under: readable slug plus a short hash so two
/// sources that slug alike never collide.
pub(crate) fn graph_import_file_name(source: &str) -> String {
    let slug: String = source
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || ch == '-' || ch == '_' {
                ch
            } else {
                '_'
            }
        })
        .collect();
    let slug = slug.trim_matches('_');
    let slug = if slug.is_empty() { "import" } else { slug };
    let slug: String = slug.chars().take(64).collect();
    let hash = blake3::hash(source.as_bytes()).to_hex();
    format!("{slug}-{}.json", &hash[..8])
}

/// Every stored import under `dir`, sorted by file name for a stable projection.
pub(crate) fn stored_graph_import_files(dir: &Path) -> Vec<PathBuf> {
    let Ok(entries) = fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut files: Vec<PathBuf> = entries
        .filter_map(|entry| entry.ok().map(|entry| entry.path()))
        .filter(|path| path.extension().and_then(|ext| ext.to_str()) == Some("json"))
        .collect();
    files.sort();
    files
}

/// Watermark parts for the stored imports, so a changed import invalidates the
/// cached projection the same way a changed source file does.
pub(crate) fn graph_import_watermark_parts(graph_db: &Path) -> Vec<String> {
    let dir = graph_imports_dir_for_db(graph_db);
    stored_graph_import_files(&dir)
        .into_iter()
        .map(|path| {
            let digest = fs::read(&path)
                .map(|bytes| blake3::hash(&bytes).to_hex().to_string())
                .unwrap_or_else(|_| "unreadable".to_string());
            let name = path
                .file_name()
                .map(|name| name.to_string_lossy().to_string())
                .unwrap_or_default();
            format!("graph_import:{name}:{digest}")
        })
        .collect()
}

/// Parse an import file in `format`, lowering adapter formats into an
/// [`EdgeFile`]. `source_override` wins over the file's own `source`, which
/// wins over `default_source`.
pub(crate) fn parse_edge_file(
    file: &Path,
    format: ImportFormat,
    source_override: Option<&str>,
    default_source: &str,
) -> Result<EdgeFile> {
    let text = fs::read_to_string(file)
        .with_context(|| format!("reading edge file {}", file.display()))?;
    let value: Value = serde_json::from_str(&text)
        .with_context(|| format!("parsing edge file {} as JSON", file.display()))?;
    let format = match format {
        ImportFormat::Auto if is_haiven_trace(&value) => ImportFormat::HaivenTrace,
        ImportFormat::Auto => ImportFormat::Edges,
        other => other,
    };
    let mut edge_file = match format {
        ImportFormat::HaivenTrace => {
            let model = file
                .parent()
                .map(|dir| dir.join("model.json"))
                .filter(|path| path.is_file())
                .and_then(|path| fs::read_to_string(path).ok())
                .and_then(|text| serde_json::from_str::<Value>(&text).ok());
            lower_haiven_trace(&value, model.as_ref())?
        }
        _ => {
            let mut parsed: EdgeFile = serde_json::from_value(value).with_context(|| {
                format!(
                    "edge file {} is not `{GRAPH_EDGES_FORMAT}` (expected {{\"nodes\"?: [...], \"edges\": [{{\"from\", \"to\", \"kind\"}}]}})",
                    file.display()
                )
            })?;
            if let Some(tag) = &parsed.format
                && tag != GRAPH_EDGES_FORMAT
            {
                bail!(
                    "edge file {} declares format `{tag}`; this tsift reads `{GRAPH_EDGES_FORMAT}`",
                    file.display()
                );
            }
            parsed.adapter = Some("edges".to_string());
            parsed
        }
    };
    edge_file.format = Some(GRAPH_EDGES_FORMAT.to_string());
    let source = source_override
        .map(str::to_string)
        .or_else(|| edge_file.source.clone())
        .unwrap_or_else(|| default_source.to_string());
    if source.trim().is_empty() {
        bail!("graph import source id must not be empty");
    }
    edge_file.source = Some(source);
    validate_edge_file(&edge_file)?;
    Ok(edge_file)
}

fn validate_edge_file(file: &EdgeFile) -> Result<()> {
    let mut ids = BTreeSet::new();
    for node in &file.nodes {
        if node.id.trim().is_empty() {
            bail!("edge file node has an empty `id`");
        }
        if !ids.insert(node.id.as_str()) {
            bail!("edge file declares node `{}` more than once", node.id);
        }
        if let Some(join) = &node.join {
            let fields = join.fields();
            if fields.name.is_none() && fields.path.is_none() {
                bail!(
                    "node `{}` has a `join` with neither `name` nor `path`",
                    node.id
                );
            }
            if fields.line.is_some() && fields.path.is_none() {
                bail!("node `{}` has a `join.line` without `join.path`", node.id);
            }
        }
    }
    for (index, edge) in file.edges.iter().enumerate() {
        if edge.from.trim().is_empty() || edge.to.trim().is_empty() {
            bail!("edge {index} has an empty `from` or `to`");
        }
        if edge.kind.trim().is_empty() {
            bail!(
                "edge {index} (`{}` -> `{}`) has an empty `kind`",
                edge.from,
                edge.to
            );
        }
    }
    Ok(())
}

fn property_string(value: &Value) -> String {
    match value {
        Value::String(text) => text.clone(),
        Value::Null => String::new(),
        other => other.to_string(),
    }
}

/// Index of the projection's joinable nodes.
struct JoinIndex {
    by_name: BTreeMap<String, Vec<usize>>,
    by_path_line: BTreeMap<(String, i64), Vec<usize>>,
    file_by_path: BTreeMap<String, usize>,
}

impl JoinIndex {
    fn build(nodes: &[GraphNode]) -> Self {
        let mut by_name: BTreeMap<String, Vec<usize>> = BTreeMap::new();
        let mut by_path_line: BTreeMap<(String, i64), Vec<usize>> = BTreeMap::new();
        let mut file_by_path = BTreeMap::new();
        for (index, node) in nodes.iter().enumerate() {
            match node.kind.as_str() {
                "symbol" => {
                    by_name.entry(node.label.clone()).or_default().push(index);
                    if let (Some(path), Some(line)) = (
                        node.properties.get("path"),
                        node.properties
                            .get("line")
                            .and_then(|line| line.parse::<i64>().ok()),
                    ) {
                        by_path_line
                            .entry((path.clone(), line))
                            .or_default()
                            .push(index);
                    }
                }
                "file" => {
                    if let Some(path) = node.properties.get("path") {
                        file_by_path.insert(path.clone(), index);
                    }
                }
                _ => {}
            }
        }
        Self {
            by_name,
            by_path_line,
            file_by_path,
        }
    }

    fn language_of(node: &GraphNode) -> Option<&str> {
        node.properties
            .get("detail")
            .and_then(|detail| detail.split_whitespace().next())
    }

    /// Indexed node ids this join lands on (empty = unmatched), trying
    /// `name` and then each alias.
    fn resolve(&self, nodes: &[GraphNode], fields: &JoinFields) -> Vec<String> {
        let matches = self.resolve_one(nodes, fields);
        if !matches.is_empty() || fields.name.is_none() {
            return matches;
        }
        for alias in &fields.aliases {
            let aliased = JoinFields {
                name: Some(alias.clone()),
                aliases: Vec::new(),
                ..fields.clone()
            };
            let matches = self.resolve_one(nodes, &aliased);
            if !matches.is_empty() {
                return matches;
            }
        }
        Vec::new()
    }

    fn resolve_one(&self, nodes: &[GraphNode], fields: &JoinFields) -> Vec<String> {
        let candidates: Vec<usize> = match (&fields.path, fields.line, &fields.name) {
            (Some(path), Some(line), _) => self
                .by_path_line
                .get(&(path.clone(), line))
                .cloned()
                .unwrap_or_default(),
            (Some(path), None, None) => self.file_by_path.get(path).copied().into_iter().collect(),
            (path, None, Some(name)) => self
                .by_name
                .get(name)
                .map(|indices| {
                    indices
                        .iter()
                        .copied()
                        .filter(|index| {
                            path.as_ref().is_none_or(|path| {
                                nodes[*index].properties.get("path") == Some(path)
                            })
                        })
                        .collect()
                })
                .unwrap_or_default(),
            (None, Some(_), _) | (None, None, None) => Vec::new(),
        };
        let mut ids: Vec<String> = candidates
            .into_iter()
            .filter(|index| {
                let node = &nodes[*index];
                fields.languages.is_empty()
                    || Self::language_of(node).is_some_and(|language| {
                        fields
                            .languages
                            .iter()
                            .any(|wanted| wanted.eq_ignore_ascii_case(language))
                    })
            })
            .filter(|index| {
                fields
                    .name
                    .as_ref()
                    .is_none_or(|name| &nodes[*index].label == name)
            })
            .map(|index| nodes[index].id.clone())
            .collect();
        ids.sort();
        ids.dedup();
        ids
    }
}

/// Stable id of an imported (non-joined) node.
pub(crate) fn graph_import_node_id(source: &str, local_id: &str) -> String {
    crate::stable_handle("gimp", &format!("{source}\u{0}{local_id}"))
}

/// Id of the summary node a source projects.
pub(crate) fn graph_import_source_node_id(source: &str) -> String {
    format!("graph_import:{source}")
}

/// Append one edge file's rows to `nodes`/`edges`, joining endpoints against the
/// nodes already present. Returns the resolution counts.
pub(crate) fn append_edge_file_rows(
    file: &EdgeFile,
    nodes: &mut Vec<GraphNode>,
    edges: &mut Vec<GraphEdge>,
) -> GraphImportStats {
    let source = file.source.clone().unwrap_or_default();
    let provenance = GraphProvenance::new(
        GRAPH_IMPORT_PROVIDER,
        file.imported_from.clone().unwrap_or_else(|| source.clone()),
    );
    let mut stats = GraphImportStats {
        source: source.clone(),
        adapter: file.adapter.clone().unwrap_or_else(|| "edges".to_string()),
        declared_nodes: file.nodes.len(),
        ..GraphImportStats::default()
    };
    let join_index = JoinIndex::build(nodes);
    let mut existing_node_ids: BTreeSet<String> =
        nodes.iter().map(|node| node.id.clone()).collect();
    let mut existing_edges: BTreeSet<(String, String, String)> = edges
        .iter()
        .map(|edge| (edge.from_id.clone(), edge.to_id.clone(), edge.kind.clone()))
        .collect();
    fn note_unmatched(stats: &mut GraphImportStats, name: &str) {
        if stats.unmatched_sample.len() < UNMATCHED_SAMPLE_LIMIT {
            stats.unmatched_sample.push(name.to_string());
        }
    }

    let mut push_imported_node =
        |nodes: &mut Vec<GraphNode>,
         id: String,
         kind: &str,
         label: &str,
         local_id: &str,
         props: &BTreeMap<String, Value>| {
            if !existing_node_ids.insert(id.clone()) {
                return;
            }
            let mut node = GraphNode::new(id, kind, label)
                .with_property("provider", GRAPH_IMPORT_PROVIDER)
                .with_property("import_source", source.as_str())
                .with_property("import_id", local_id)
                .with_provenance(provenance.clone());
            for (key, value) in props {
                node = node.with_property(key.clone(), property_string(value));
            }
            nodes.push(node);
        };

    // Resolve declared nodes first: each maps to indexed node ids or to itself.
    let mut resolved: BTreeMap<String, Vec<String>> = BTreeMap::new();
    let declared: Vec<EdgeFileNode> = file.nodes.clone();
    for node in &declared {
        let label = node.label.clone().unwrap_or_else(|| node.id.clone());
        let kind = node.kind.clone().unwrap_or_else(|| "imported".to_string());
        if let Some(join) = &node.join {
            let fields = join.fields();
            let matches = join_index.resolve(nodes, &fields);
            if !matches.is_empty() && matches.len() <= MAX_JOIN_MATCHES {
                if matches.len() > 1 {
                    stats.ambiguous_joins += 1;
                }
                stats.joined_nodes += 1;
                resolved.insert(node.id.clone(), matches);
                continue;
            }
            if matches.len() > MAX_JOIN_MATCHES {
                stats.ambiguous_joins += 1;
            }
            stats.unmatched_joins += 1;
            let hint = fields
                .name
                .clone()
                .or_else(|| {
                    fields.path.as_ref().map(|path| match fields.line {
                        Some(line) => format!("{path}:{line}"),
                        None => path.clone(),
                    })
                })
                .unwrap_or_else(|| node.id.clone());
            note_unmatched(&mut stats, &hint);
        }
        let id = graph_import_node_id(&source, &node.id);
        push_imported_node(nodes, id.clone(), &kind, &label, &node.id, &node.properties);
        resolved.insert(node.id.clone(), vec![id]);
    }

    // Undeclared endpoints join by name, or become external nodes.
    let mut endpoint =
        |nodes: &mut Vec<GraphNode>, stats: &mut GraphImportStats, name: &str| -> Vec<String> {
            if let Some(ids) = resolved.get(name) {
                return ids.clone();
            }
            let fields = JoinFields {
                name: Some(name.to_string()),
                ..JoinFields::default()
            };
            let matches = join_index.resolve(nodes, &fields);
            let ids = if !matches.is_empty() && matches.len() <= MAX_JOIN_MATCHES {
                if matches.len() > 1 {
                    stats.ambiguous_joins += 1;
                }
                stats.matched_endpoints += 1;
                matches
            } else {
                if matches.len() > MAX_JOIN_MATCHES {
                    stats.ambiguous_joins += 1;
                }
                stats.external_endpoints += 1;
                note_unmatched(stats, name);
                let id = graph_import_node_id(&source, name);
                push_imported_node(
                    nodes,
                    id.clone(),
                    GRAPH_IMPORT_EXTERNAL_KIND,
                    name,
                    name,
                    &BTreeMap::new(),
                );
                vec![id]
            };
            resolved.insert(name.to_string(), ids.clone());
            ids
        };

    for edge in &file.edges {
        let from_ids = endpoint(nodes, &mut stats, &edge.from);
        let to_ids = endpoint(nodes, &mut stats, &edge.to);
        for from in &from_ids {
            for to in &to_ids {
                if from == to {
                    continue;
                }
                if !existing_edges.insert((from.clone(), to.clone(), edge.kind.clone())) {
                    stats.duplicate_edges += 1;
                    continue;
                }
                let mut projected = GraphEdge::new(from.clone(), to.clone(), edge.kind.clone())
                    .with_property("provider", GRAPH_IMPORT_PROVIDER)
                    .with_property("import_source", source.as_str())
                    .with_provenance(provenance.clone());
                for (key, value) in &edge.properties {
                    projected = projected.with_property(key.clone(), property_string(value));
                }
                edges.push(projected);
                stats.edges += 1;
            }
        }
    }

    let summary_id = graph_import_source_node_id(&source);
    if !nodes.iter().any(|node| node.id == summary_id) {
        let mut summary = GraphNode::new(summary_id, GRAPH_IMPORT_SOURCE_KIND, source.as_str())
            .with_property("provider", GRAPH_IMPORT_PROVIDER)
            .with_property("import_source", source.as_str())
            .with_property("adapter", stats.adapter.as_str())
            .with_property("declared_nodes", stats.declared_nodes.to_string())
            .with_property("edges", stats.edges.to_string())
            .with_property("duplicate_edges", stats.duplicate_edges.to_string())
            .with_property("joined_nodes", stats.joined_nodes.to_string())
            .with_property("unmatched_joins", stats.unmatched_joins.to_string())
            .with_property("ambiguous_joins", stats.ambiguous_joins.to_string())
            .with_property("matched_endpoints", stats.matched_endpoints.to_string())
            .with_property("external_endpoints", stats.external_endpoints.to_string())
            .with_provenance(provenance);
        if let Some(from) = &file.imported_from {
            summary = summary.with_property("imported_from", from.as_str());
        }
        nodes.push(summary);
    }
    stats
}

/// Append every stored import for `graph_db` to a projection being built.
/// A stored file that no longer parses is skipped with a warning so one bad
/// import never blocks the refresh of everything else.
pub(crate) fn append_stored_graph_imports(
    graph_db: &Path,
    nodes: &mut Vec<GraphNode>,
    edges: &mut Vec<GraphEdge>,
    warnings: &mut Vec<String>,
) -> Vec<GraphImportStats> {
    let dir = graph_imports_dir_for_db(graph_db);
    let mut all = Vec::new();
    for path in stored_graph_import_files(&dir) {
        let parsed = fs::read_to_string(&path)
            .map_err(anyhow::Error::from)
            .and_then(|text| serde_json::from_str::<EdgeFile>(&text).map_err(Into::into))
            .and_then(|file| validate_edge_file(&file).map(|()| file));
        match parsed {
            Ok(file) => all.push(append_edge_file_rows(&file, nodes, edges)),
            Err(err) => warnings.push(format!(
                "skipped unreadable graph import {}: {err}",
                path.display()
            )),
        }
    }
    all
}

// ---------------------------------------------------------------------------
// `tsift graph-db import`
// ---------------------------------------------------------------------------

impl From<crate::cli::GraphImportFormat> for ImportFormat {
    fn from(format: crate::cli::GraphImportFormat) -> Self {
        match format {
            crate::cli::GraphImportFormat::Auto => ImportFormat::Auto,
            crate::cli::GraphImportFormat::Edges => ImportFormat::Edges,
            crate::cli::GraphImportFormat::HaivenTrace => ImportFormat::HaivenTrace,
        }
    }
}

pub(crate) struct GraphDbImportOptions<'a> {
    pub root: &'a Path,
    pub path: &'a Path,
    pub scope: Option<&'a str>,
    pub file: Option<&'a Path>,
    pub source: Option<&'a str>,
    pub format: ImportFormat,
    pub remove: bool,
    pub list: bool,
}

#[derive(Debug, Serialize)]
pub(crate) struct GraphDbImportRefresh {
    pub upserted_nodes: usize,
    pub upserted_edges: usize,
    pub unchanged_nodes: usize,
    pub unchanged_edges: usize,
    pub deleted_nodes: usize,
    pub deleted_edges: usize,
}

#[derive(Debug, Serialize)]
pub(crate) struct GraphDbImportEntry {
    pub source: String,
    pub stored_file: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub imported_from: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub adapter: Option<String>,
    pub file_nodes: usize,
    pub file_edges: usize,
    /// Resolution counts as projected into graph.db (absent until a refresh).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub projected: Option<BTreeMap<String, String>>,
}

#[derive(Debug, Serialize)]
pub(crate) struct GraphDbImportReport {
    pub root: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub scope: Option<String>,
    pub graph_db: String,
    pub operation: String,
    pub imports: Vec<GraphDbImportEntry>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub refresh: Option<GraphDbImportRefresh>,
    pub next_commands: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub warnings: Vec<String>,
}

/// A relative FILE resolves against the cwd, falling back to `--path`.
fn resolve_import_file(root: &Path, file: &Path) -> PathBuf {
    if file.is_relative() && !file.exists() && root.join(file).exists() {
        root.join(file)
    } else {
        file.to_path_buf()
    }
}

fn default_import_source(root: &Path, file: &Path) -> String {
    let absolute = fs::canonicalize(file).unwrap_or_else(|_| file.to_path_buf());
    absolute
        .strip_prefix(root)
        .map(|relative| relative.to_string_lossy().to_string())
        .unwrap_or_else(|_| absolute.to_string_lossy().to_string())
}

fn read_stored(path: &Path) -> Option<EdgeFile> {
    fs::read_to_string(path)
        .ok()
        .and_then(|text| serde_json::from_str::<EdgeFile>(&text).ok())
}

fn projected_counts(graph_db: &Path, source: &str) -> Option<BTreeMap<String, String>> {
    use tsift_sqlite::GraphStore as _;
    if !graph_db.exists() {
        return None;
    }
    let store = tsift_sqlite::SqliteGraphStore::open_read_only_resilient(graph_db).ok()?;
    let node = store.node(&graph_import_source_node_id(source)).ok()??;
    Some(
        node.properties
            .into_iter()
            .filter(|(key, _)| !matches!(key.as_str(), "provider" | "import_source"))
            .collect(),
    )
}

fn import_entry(graph_db: &Path, stored: &Path, file: &EdgeFile) -> GraphDbImportEntry {
    let source = file.source.clone().unwrap_or_default();
    GraphDbImportEntry {
        projected: projected_counts(graph_db, &source),
        source,
        stored_file: stored.display().to_string(),
        imported_from: file.imported_from.clone(),
        adapter: file.adapter.clone(),
        file_nodes: file.nodes.len(),
        file_edges: file.edges.len(),
    }
}

fn write_stored_atomically(path: &Path, file: &EdgeFile) -> Result<()> {
    let dir = path
        .parent()
        .context("graph import path has no parent directory")?;
    fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    let tmp = dir.join(format!(
        ".{}.{}.tmp",
        path.file_name()
            .map(|name| name.to_string_lossy().to_string())
            .unwrap_or_default(),
        std::process::id()
    ));
    fs::write(&tmp, serde_json::to_vec_pretty(file)?)
        .with_context(|| format!("writing {}", tmp.display()))?;
    fs::rename(&tmp, path).with_context(|| format!("installing {}", path.display()))?;
    Ok(())
}

pub(crate) fn cmd_graph_db_import(
    options: GraphDbImportOptions<'_>,
    format: crate::output::OutputFormat,
) -> Result<()> {
    let root = options.root;
    let scope = options.scope;
    let graph_db = crate::graph_substrate_db_path(root, scope);
    let imports_dir = graph_imports_dir_for_db(&graph_db);
    let scope_arg = crate::graph_db_scope_arg(scope);
    let root_arg = crate::shell_quote(root.to_string_lossy().as_ref());
    let mut warnings = Vec::new();

    let (operation, changed) = if options.list {
        ("list", false)
    } else if options.remove {
        let source = match (options.source, options.file) {
            (Some(source), _) => source.to_string(),
            (None, Some(file)) => {
                let file = resolve_import_file(root, file);
                parse_edge_file(
                    &file,
                    options.format,
                    None,
                    &default_import_source(root, &file),
                )?
                .source
                .unwrap_or_default()
            }
            (None, None) => bail!("graph-db import --remove needs --source <id> or FILE"),
        };
        let stored = imports_dir.join(graph_import_file_name(&source));
        if !stored.exists() {
            bail!(
                "no stored graph import for source `{source}` under {}",
                imports_dir.display()
            );
        }
        fs::remove_file(&stored).with_context(|| format!("removing {}", stored.display()))?;
        ("remove", true)
    } else {
        let file = resolve_import_file(
            root,
            options
                .file
                .context("graph-db import needs FILE (or --list / --remove)")?,
        );
        let file = file.as_path();
        let mut edge_file = parse_edge_file(
            file,
            options.format,
            options.source,
            &default_import_source(root, file),
        )?;
        edge_file.imported_from = Some(default_import_source(root, file));
        let source = edge_file.source.clone().unwrap_or_default();
        let stored = imports_dir.join(graph_import_file_name(&source));
        write_stored_atomically(&stored, &edge_file)?;
        ("import", true)
    };

    let refresh = if changed {
        crate::ensure_graph_refresh_work_is_bounded(root, options.path, scope)?;
        let (graph, refresh) = crate::write_traversal_graph_store(root, options.path, scope)?;
        warnings.extend(graph.warnings);
        Some(GraphDbImportRefresh {
            upserted_nodes: refresh.upserted_nodes,
            upserted_edges: refresh.upserted_edges,
            unchanged_nodes: refresh.unchanged_nodes,
            unchanged_edges: refresh.unchanged_edges,
            deleted_nodes: refresh.deleted_nodes,
            deleted_edges: refresh.deleted_edges,
        })
    } else {
        None
    };

    let imports: Vec<GraphDbImportEntry> = stored_graph_import_files(&imports_dir)
        .into_iter()
        .filter_map(|stored| match read_stored(&stored) {
            Some(file) => Some(import_entry(&graph_db, &stored, &file)),
            None => {
                warnings.push(format!(
                    "unreadable stored graph import {}",
                    stored.display()
                ));
                None
            }
        })
        .filter(|entry| {
            options.list
                || options.remove
                || options.source.is_none_or(|source| entry.source == source)
        })
        .collect();

    let mut next_commands = vec![
        format!(
            "tsift graph-db --path {root_arg}{scope_arg} kind {GRAPH_IMPORT_SOURCE_KIND} --json"
        ),
        format!("tsift graph-db --path {root_arg}{scope_arg} import --list --json"),
    ];
    if imports
        .iter()
        .any(|entry| entry.adapter.as_deref() == Some("haiven-trace"))
    {
        next_commands.push(format!(
            "tsift graph-db --path {root_arg}{scope_arg} kind symbol --property ref_id=<ContractName> --json  # find the contract node id"
        ));
        next_commands.push(format!(
            "tsift graph-db --path {root_arg}{scope_arg} neighborhood <contract-node-id> --depth 2 --json  # declarations + carrying frames + conformance scenarios"
        ));
    }

    let report = GraphDbImportReport {
        root: root.to_string_lossy().to_string(),
        scope: scope.map(str::to_string),
        graph_db: graph_db.display().to_string(),
        operation: operation.to_string(),
        imports,
        refresh,
        next_commands: next_commands.clone(),
        warnings: crate::dedupe_preserve_order(warnings),
    };

    if format.json_output {
        let mut metrics = vec![crate::envelope_metric("imports", report.imports.len())];
        let totals = |key: &str| -> usize {
            report
                .imports
                .iter()
                .filter_map(|entry| entry.projected.as_ref()?.get(key)?.parse::<usize>().ok())
                .sum()
        };
        metrics.push(crate::envelope_metric("edges", totals("edges")));
        metrics.push(crate::envelope_metric(
            "joined_nodes",
            totals("joined_nodes"),
        ));
        metrics.push(crate::envelope_metric(
            "external_endpoints",
            totals("external_endpoints"),
        ));
        crate::print_json_or_envelope(
            &report,
            &format,
            "graph-db",
            operation,
            crate::output::ToolEnvelopeSummary {
                text: format!(
                    "graph-db import {operation}: {} stored import(s)",
                    report.imports.len()
                ),
                metrics,
            },
            false,
            next_commands,
        )
    } else {
        println!("graph-db import {operation}: {}", report.graph_db);
        for entry in &report.imports {
            println!(
                "  {} ({}) nodes={} edges={}",
                entry.source,
                entry.adapter.as_deref().unwrap_or("edges"),
                entry.file_nodes,
                entry.file_edges
            );
            if let Some(projected) = &entry.projected {
                let counts: Vec<String> = projected
                    .iter()
                    .filter(|(key, _)| {
                        matches!(
                            key.as_str(),
                            "edges"
                                | "joined_nodes"
                                | "unmatched_joins"
                                | "ambiguous_joins"
                                | "matched_endpoints"
                                | "external_endpoints"
                                | "duplicate_edges"
                        )
                    })
                    .map(|(key, value)| format!("{key}={value}"))
                    .collect();
                println!("    projected: {}", counts.join(" "));
            }
        }
        for warning in &report.warnings {
            println!("  warning: {warning}");
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// haiven-sdk codegen/trace.json adapter
// ---------------------------------------------------------------------------

/// A haiven-sdk `codegen/trace.json`: model sections keyed by node, plus a
/// `conformance` coverage block naming its suites.
pub(crate) fn is_haiven_trace(value: &Value) -> bool {
    value.get("edges").is_none()
        && value.get("contracts").is_some_and(Value::is_object)
        && value
            .get("conformance")
            .and_then(|conformance| conformance.get("suites"))
            .is_some_and(Value::is_array)
}

/// Index languages that hold API contracts (`#sdktsiftcontracts`).
fn contract_languages() -> Vec<String> {
    vec!["json".to_string(), "yaml".to_string()]
}

struct TraceLowering {
    nodes: BTreeMap<String, EdgeFileNode>,
    edges: Vec<EdgeFileEdge>,
}

impl TraceLowering {
    fn node(
        &mut self,
        id: String,
        kind: &str,
        label: &str,
        join: Option<JoinSpec>,
        properties: BTreeMap<String, Value>,
    ) -> String {
        self.nodes
            .entry(id.clone())
            .or_insert_with(|| EdgeFileNode {
                id: id.clone(),
                kind: Some(kind.to_string()),
                label: Some(label.to_string()),
                join,
                properties,
            });
        id
    }

    fn edge(&mut self, from: &str, to: &str, kind: &str, properties: BTreeMap<String, Value>) {
        self.edges.push(EdgeFileEdge {
            from: from.to_string(),
            to: to.to_string(),
            kind: kind.to_string(),
            properties,
        });
    }

    /// One `sdk_declaration` per language, each joined to the indexed symbol
    /// declared at that `path:line`, and a `declared_in` edge from `owner`.
    fn declarations(&mut self, owner: &str, surface: &str, name: &str, declared: Option<&Value>) {
        let Some(declared) = declared.and_then(Value::as_object) else {
            return;
        };
        for (language, location) in declared {
            let Some(location) = location.as_str() else {
                continue;
            };
            let (path, line) = match location.rsplit_once(':') {
                Some((path, line)) => (path.to_string(), line.parse::<i64>().ok()),
                None => (location.to_string(), None),
            };
            let mut properties = BTreeMap::new();
            properties.insert("language".to_string(), Value::from(language.as_str()));
            properties.insert("surface".to_string(), Value::from(surface));
            properties.insert("location".to_string(), Value::from(location));
            properties.insert("path".to_string(), Value::from(path.as_str()));
            if let Some(line) = line {
                properties.insert("line".to_string(), Value::from(line));
            }
            let id = self.node(
                format!("decl:{surface}:{name}:{language}"),
                "sdk_declaration",
                &format!("{name} ({language})"),
                Some(JoinSpec::Spec(JoinFields {
                    name: None,
                    languages: Vec::new(),
                    path: Some(path),
                    line,
                    aliases: Vec::new(),
                })),
                properties,
            );
            let mut edge_properties = BTreeMap::new();
            edge_properties.insert("language".to_string(), Value::from(language.as_str()));
            self.edge(owner, &id, "declared_in", edge_properties);
        }
    }

    fn scenarios(&mut self, owner: &str, conformance: Option<&Value>) {
        for scenario in conformance
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
        {
            let suite = scenario.split_once('#').map(|(suite, _)| suite);
            let mut properties = BTreeMap::new();
            if let Some(suite) = suite {
                properties.insert("suite".to_string(), Value::from(suite));
            }
            let id = self.node(
                format!("scenario:{scenario}"),
                "conformance_scenario",
                scenario,
                None,
                properties,
            );
            self.edge(owner, &id, "exercised_by", BTreeMap::new());
            if let Some(suite) = suite {
                let suite_id = self.suite(suite);
                self.edge(&id, &suite_id, "in_suite", BTreeMap::new());
            }
        }
    }

    fn suite(&mut self, suite: &str) -> String {
        self.node(
            format!("suite:{suite}"),
            "conformance_suite",
            suite,
            None,
            BTreeMap::new(),
        )
    }
}

fn contract_join(name: &str) -> Option<JoinSpec> {
    Some(JoinSpec::Spec(JoinFields {
        name: Some(name.to_string()),
        languages: contract_languages(),
        path: None,
        line: None,
        aliases: Vec::new(),
    }))
}

/// Lower a haiven-sdk `codegen/trace.json` into the generic edge shape.
///
/// | trace section | node kind | joins to (indexed contracts) | edges |
/// |---|---|---|---|
/// | `contracts.<Stem>` | `contract` | JSON Schema titled `<Stem>` | `declared_in` → `sdk_declaration`, `declares_record` → `contract_record` |
/// | `vocabularies.<Name>` | `vocabulary` | symbol `<Name>` | `declared_in` |
/// | `rest."<METHOD> <path>"` | `rest_operation` | OpenAPI operation `<METHOD> <path>`, else `operationId` (from `model.json`) | `declared_in`, `exercised_by` |
/// | `ws.requests.<op>` / `ws.events.<type>` | `ws_request` / `ws_event` | AsyncAPI operation `<op>` | `declared_in`, `exercised_by` |
/// | scenario `suite#id` | `conformance_scenario` | — | `in_suite` → `conformance_suite` |
///
/// Each `sdk_declaration` joins to the indexed symbol at its `path:line`. When
/// the sibling `model.json` is present, payload links are added too: a
/// contract `carried_by` each WebSocket request/event whose payload it is, and
/// a REST component `used_by` each operation whose body or response names it,
/// so a contract reaches its conformance scenarios in one traversal.
pub(crate) fn lower_haiven_trace(trace: &Value, model: Option<&Value>) -> Result<EdgeFile> {
    if !is_haiven_trace(trace) {
        bail!("not a haiven-sdk codegen trace (expected `contracts` and `conformance.suites`)");
    }
    let mut lowering = TraceLowering {
        nodes: BTreeMap::new(),
        edges: Vec::new(),
    };

    for (name, entry) in trace
        .get("vocabularies")
        .and_then(Value::as_object)
        .into_iter()
        .flatten()
    {
        let id = lowering.node(
            format!("vocabulary:{name}"),
            "vocabulary",
            name,
            contract_join(name),
            BTreeMap::new(),
        );
        lowering.declarations(&id, "vocabulary", name, entry.get("declared"));
    }

    for (stem, entry) in trace
        .get("contracts")
        .and_then(Value::as_object)
        .into_iter()
        .flatten()
    {
        let id = lowering.node(
            format!("contract:{stem}"),
            "contract",
            stem,
            contract_join(stem),
            BTreeMap::new(),
        );
        lowering.declarations(&id, "contracts", stem, entry.get("declared"));
        for (record, declared) in entry
            .get("records")
            .and_then(Value::as_object)
            .into_iter()
            .flatten()
        {
            let qualified = format!("{stem}.{record}");
            let record_id = lowering.node(
                format!("contract_record:{qualified}"),
                "contract_record",
                &qualified,
                None,
                BTreeMap::new(),
            );
            lowering.edge(&id, &record_id, "declares_record", BTreeMap::new());
            lowering.declarations(&record_id, "contracts", &qualified, Some(declared));
        }
    }

    let model_rest = model
        .and_then(|model| model.get("rest"))
        .and_then(|rest| rest.get("operations"))
        .and_then(Value::as_object);
    for (operation, entry) in trace
        .get("rest")
        .and_then(Value::as_object)
        .into_iter()
        .flatten()
    {
        let modeled = model_rest.and_then(|operations| operations.get(operation));
        let operation_id = modeled
            .and_then(|modeled| modeled.get("name"))
            .and_then(Value::as_str);
        let mut properties = BTreeMap::new();
        if let Some(operation_id) = operation_id {
            properties.insert("operation_id".to_string(), Value::from(operation_id));
        }
        let id = lowering.node(
            format!("rest:{operation}"),
            "rest_operation",
            operation,
            // The contract index names an operation by `operationId` when the
            // description has one and `METHOD /path` otherwise; try both.
            Some(JoinSpec::Spec(JoinFields {
                name: Some(operation.clone()),
                languages: contract_languages(),
                aliases: operation_id.map(str::to_string).into_iter().collect(),
                ..JoinFields::default()
            })),
            properties,
        );
        lowering.declarations(&id, "rest", operation, entry.get("declared"));
        lowering.scenarios(&id, entry.get("conformance"));
        for component in modeled.map(rest_component_refs).into_iter().flatten() {
            let component_id = lowering.node(
                format!("rest_component:{component}"),
                "rest_component",
                &component,
                contract_join(&component),
                BTreeMap::new(),
            );
            lowering.edge(&component_id, &id, "used_by", BTreeMap::new());
        }
    }

    let ws = trace.get("ws");
    for (section, kind) in [("requests", "ws_request"), ("events", "ws_event")] {
        for (name, entry) in ws
            .and_then(|ws| ws.get(section))
            .and_then(Value::as_object)
            .into_iter()
            .flatten()
        {
            let id = lowering.node(
                format!("{kind}:{name}"),
                kind,
                name,
                contract_join(name),
                BTreeMap::new(),
            );
            lowering.declarations(&id, "ws", name, entry.get("declared"));
            lowering.scenarios(&id, entry.get("conformance"));
        }
    }

    // model.json payload links: contract -> the WebSocket frames that carry it.
    let model_ws = model.and_then(|model| model.get("ws"));
    let mut payloads: Vec<(String, String)> = Vec::new();
    for (event, contract) in model_ws
        .and_then(|ws| ws.get("events"))
        .and_then(Value::as_object)
        .into_iter()
        .flatten()
    {
        if let Some(contract) = contract.as_str() {
            payloads.push((contract.to_string(), format!("ws_event:{event}")));
        }
    }
    for request in model_ws
        .and_then(|ws| ws.get("requests"))
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        if let (Some(operation), Some(contract)) = (
            request.get("operation").and_then(Value::as_str),
            request.get("payload").and_then(Value::as_str),
        ) {
            payloads.push((contract.to_string(), format!("ws_request:{operation}")));
        }
    }
    for (contract, frame) in payloads {
        let contract_id = format!("contract:{contract}");
        if lowering.nodes.contains_key(&contract_id) && lowering.nodes.contains_key(&frame) {
            lowering.edge(&contract_id, &frame, "carried_by", BTreeMap::new());
        }
    }

    for suite in trace
        .get("conformance")
        .and_then(|conformance| conformance.get("suites"))
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
    {
        lowering.suite(suite);
    }

    // Declared nodes are always projected, so a suite no scenario names still
    // shows up as a `conformance_suite` node.
    Ok(EdgeFile {
        format: Some(GRAPH_EDGES_FORMAT.to_string()),
        source: None,
        imported_from: None,
        adapter: Some("haiven-trace".to_string()),
        nodes: lowering.nodes.into_values().collect(),
        edges: lowering.edges,
    })
}

/// `ref:Name` component names a model REST operation's body or response uses.
fn rest_component_refs(operation: &Value) -> Vec<String> {
    let mut refs = BTreeSet::new();
    let mut stack = vec![operation];
    while let Some(value) = stack.pop() {
        match value {
            Value::String(text) => {
                if let Some(name) = text.strip_prefix("ref:") {
                    refs.insert(name.to_string());
                }
            }
            Value::Array(items) => stack.extend(items),
            Value::Object(map) => stack.extend(map.values()),
            _ => {}
        }
    }
    refs.into_iter().collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn symbol(id: &str, name: &str, language: &str, path: &str, line: i64) -> GraphNode {
        GraphNode::new(id, "symbol", name)
            .with_property("path", path)
            .with_property("line", line.to_string())
            .with_property("detail", format!("{language} schema"))
    }

    #[test]
    fn generic_file_joins_by_name_and_counts_externals() {
        let file: EdgeFile = serde_json::from_value(serde_json::json!({
            "source": "s",
            "nodes": [
                {"id": "c", "kind": "contract", "join": {"name": "Foo", "languages": ["json"]}},
                {"id": "missing", "kind": "contract", "join": "Nope"}
            ],
            "edges": [
                {"from": "c", "to": "decl-ts", "kind": "declared_in"},
                {"from": "Bar", "to": "c", "kind": "uses", "properties": {"weight": 2}},
                {"from": "missing", "to": "c", "kind": "uses"}
            ]
        }))
        .unwrap();
        let mut nodes = vec![
            symbol("gsym-foo", "Foo", "json", "schemas/foo.json", 1),
            symbol("gsym-foo-ts", "Foo", "typescript", "foo.ts", 3),
            symbol("gsym-bar", "Bar", "rust", "bar.rs", 9),
        ];
        let mut edges = Vec::new();
        let stats = append_edge_file_rows(&file, &mut nodes, &mut edges);
        assert_eq!(stats.joined_nodes, 1);
        assert_eq!(stats.unmatched_joins, 1);
        assert_eq!(stats.matched_endpoints, 1, "Bar joins by name");
        assert_eq!(
            stats.external_endpoints, 1,
            "decl-ts is undeclared and unmatched"
        );
        assert_eq!(stats.edges, 3);
        assert!(edges.iter().any(|edge| {
            edge.from_id == "gsym-foo"
                && edge.kind == "declared_in"
                && nodes
                    .iter()
                    .any(|node| node.id == edge.to_id && node.kind == GRAPH_IMPORT_EXTERNAL_KIND)
        }));
        assert!(
            edges
                .iter()
                .any(|edge| edge.from_id == "gsym-bar" && edge.to_id == "gsym-foo")
        );
        assert!(
            !edges
                .iter()
                .any(|edge| edge.from_id == "gsym-foo-ts" || edge.to_id == "gsym-foo-ts"),
            "the language filter keeps the TypeScript Foo out"
        );
        let summary = nodes
            .iter()
            .find(|node| node.id == graph_import_source_node_id("s"))
            .expect("summary node");
        assert_eq!(summary.properties["external_endpoints"], "1");
    }

    #[test]
    fn path_line_join_lands_on_the_declaring_symbol() {
        let fields = JoinFields {
            path: Some("foo.ts".to_string()),
            line: Some(3),
            ..JoinFields::default()
        };
        let nodes = vec![symbol("gsym-foo-ts", "Foo", "typescript", "foo.ts", 3)];
        let index = JoinIndex::build(&nodes);
        assert_eq!(index.resolve(&nodes, &fields), vec!["gsym-foo-ts"]);
    }

    #[test]
    fn aliases_are_tried_when_the_name_matches_nothing() {
        let nodes = vec![symbol("gsym-op", "listApps", "json", "openapi.json", 7)];
        let index = JoinIndex::build(&nodes);
        let fields = JoinFields {
            name: Some("GET /apps".to_string()),
            languages: vec!["json".to_string()],
            aliases: vec!["listApps".to_string()],
            ..JoinFields::default()
        };
        assert_eq!(index.resolve(&nodes, &fields), vec!["gsym-op"]);
    }

    #[test]
    fn malformed_files_are_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("bad.json");
        fs::write(
            &path,
            r#"{"edges": [{"from": "a", "to": "", "kind": "k"}]}"#,
        )
        .unwrap();
        assert!(parse_edge_file(&path, ImportFormat::Auto, None, "bad").is_err());
        fs::write(&path, r#"{"format": "other/v9", "edges": []}"#).unwrap();
        assert!(parse_edge_file(&path, ImportFormat::Auto, None, "bad").is_err());
        fs::write(
            &path,
            r#"{"nodes": [{"id": "a"}, {"id": "a"}], "edges": []}"#,
        )
        .unwrap();
        assert!(parse_edge_file(&path, ImportFormat::Auto, None, "bad").is_err());
    }

    #[test]
    fn file_names_are_stable_and_distinct() {
        assert_eq!(
            graph_import_file_name("codegen/trace.json"),
            graph_import_file_name("codegen/trace.json")
        );
        assert_ne!(graph_import_file_name("a/b"), graph_import_file_name("a_b"));
        assert!(graph_import_file_name("codegen/trace.json").starts_with("codegen_trace_json-"));
    }
}
