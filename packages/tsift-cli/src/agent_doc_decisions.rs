//! agent-doc decision-graph adapter for `graph-db import` (`#decisiongraphtsift`).
//!
//! agent-doc caches each session document's typed decision graph
//! (`#decisiongraph`) in its controller database, `.agent-doc/state.db`:
//! `answer_ledger_nodes` (questions, decisions, revisions, items, evidence)
//! and `answer_ledger_edges` (`answers`, `supersedes`, `amends`,
//! `applies_to`, `depends_on`, ...), both keyed by `document_id`. agent-doc
//! builds and enforces that graph one document at a time; this adapter lowers
//! the cached rows of *every* document into one `tsift-graph-edges/v1` import
//! so `graph-db` can traverse decisions across documents.
//!
//! The contract is deliberately one-sided:
//!
//! - **Read-only.** The database is opened `SQLITE_OPEN_READ_ONLY` with
//!   `query_only`, read inside one snapshot transaction, and never created,
//!   migrated, or checkpointed. A missing file or table is an import error,
//!   never a write.
//! - **Optional.** Nothing in agent-doc depends on this import; it is a
//!   point-in-time snapshot the operator refreshes by re-importing.
//! - **Schema-versioned.** The reader recognizes two cache shapes by their
//!   columns, `agent-doc/answer-ledger-edges/v1` (edges only, before
//!   `#decisiongraph`) and `agent-doc/decision-graph/v2` (typed nodes plus
//!   edge provenance), records which one it read on the import's summary
//!   node, and refuses a table whose required columns are missing instead of
//!   guessing.
//!
//! Cross-document identity: decisions, revisions and other reasoning nodes
//! are local to the document whose ledger produced them. Questions, items and
//! evidence are named by ids that mean the same thing in every document (a
//! backlog id `#foo`, a commit `commit:<sha>`, a response `r-<hex>`), so one
//! node stands for each such id and links to every document that mentions it
//! (`mentioned_in`). An id a document only references as an item (an
//! `[#id]` it depends on, an item placeholder) while exactly one other
//! document owns it as a decision resolves to that decision, the cross-document
//! counterpart of agent-doc's own in-document rule that a decision outranks an
//! item placeholder. Each document becomes a node that joins the indexed
//! `file` node of the session document when that file is indexed.
//!
//! Backlog join (`#decisionitembacklogjoin`): an item id is a backlog id, so
//! each shared node (`agent_doc_item`, or the question an item was outranked
//! to) carries a `link` to the indexed `backlog` nodes with the same `ref_id`. The refresh resolves it into a
//! `backlog -decision_item-> agent_doc_item` edge per matching backlog node,
//! and `graph-db evidence <backlog id>` walks from that item back along the
//! decision graph's edges to the decisions, revisions and questions about it.

use anyhow::{Context, Result, bail};
use rusqlite::{Connection, OpenFlags};
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::graph_import::{EdgeFile, EdgeFileEdge, EdgeFileNode, JoinFields, JoinSpec, LinkSpec};

/// Adapter name recorded on the import.
pub(crate) const AGENT_DOC_DECISIONS_ADAPTER: &str = "agent-doc-decisions";
/// Source id used unless `--source` names another.
pub(crate) const AGENT_DOC_DECISIONS_SOURCE: &str = "agent-doc-decisions";
/// agent-doc's controller database, relative to the project root.
pub(crate) const AGENT_DOC_STATE_DB: &str = ".agent-doc/state.db";
/// Cache shape before `#decisiongraph`: edges only, no provenance.
pub(crate) const SCHEMA_ANSWER_LEDGER_EDGES_V1: &str = "agent-doc/answer-ledger-edges/v1";
/// Cache shape since `#decisiongraph`: typed nodes plus edge provenance.
pub(crate) const SCHEMA_DECISION_GRAPH_V2: &str = "agent-doc/decision-graph/v2";

const SQLITE_HEADER: &[u8; 16] = b"SQLite format 3\0";
/// The controller writes `state.db` continuously; a reader waits briefly for
/// a writer's lock rather than failing, and never holds the snapshot longer
/// than the few SELECTs below.
const BUSY_TIMEOUT: Duration = Duration::from_secs(2);
const DOCUMENT_KIND: &str = "agent_doc_document";
/// Projection kind of a session document's backlog item.
const BACKLOG_KIND: &str = "backlog";
/// Edge from an indexed `backlog` node to the `agent_doc_item` with its id.
pub(crate) const BACKLOG_ITEM_EDGE: &str = "decision_item";
const KIND_PREFIX: &str = "agent_doc_";

const EDGE_TABLE: &str = "answer_ledger_edges";
const NODE_TABLE: &str = "answer_ledger_nodes";
const DOCUMENT_TABLE: &str = "documents";
const EDGE_REQUIRED: [&str; 4] = ["document_id", "from_id", "kind", "to_id"];
const NODE_REQUIRED: [&str; 4] = ["document_id", "node_id", "kind", "label"];
/// Entries a document-name walk visits before giving up on the rest.
const DOCUMENT_WALK_LIMIT: usize = 500_000;

/// Whether `path` starts with the SQLite file header.
pub(crate) fn is_sqlite_file(path: &Path) -> bool {
    let mut header = [0_u8; 16];
    fs::File::open(path)
        .and_then(|mut file| file.read_exact(&mut header))
        .is_ok_and(|()| &header == SQLITE_HEADER)
}

/// agent-doc's `state.db` under `root`.
pub(crate) fn default_state_db(root: &Path) -> PathBuf {
    root.join(AGENT_DOC_STATE_DB)
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct CachedNode {
    document_id: String,
    id: String,
    kind: String,
    label: String,
    open: Option<bool>,
    location: Option<String>,
    first_seen_at: Option<i64>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct CachedEdge {
    document_id: String,
    from: String,
    kind: String,
    to: String,
    source: Option<String>,
    clause: Option<String>,
}

#[derive(Debug, Default)]
struct Cache {
    schema: &'static str,
    nodes: Vec<CachedNode>,
    edges: Vec<CachedEdge>,
    /// `document_id -> canonical_path` from agent-doc's `documents` table.
    paths: BTreeMap<String, String>,
}

fn table_columns(conn: &Connection, table: &str) -> Result<Option<BTreeSet<String>>> {
    let exists: bool = conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = ?1)",
        [table],
        |row| row.get(0),
    )?;
    if !exists {
        return Ok(None);
    }
    let mut statement = conn.prepare("SELECT name FROM pragma_table_info(?1)")?;
    let columns = statement
        .query_map([table], |row| row.get::<_, String>(0))?
        .collect::<rusqlite::Result<BTreeSet<String>>>()?;
    Ok(Some(columns))
}

fn require_columns(
    state_db: &Path,
    table: &str,
    columns: &BTreeSet<String>,
    required: &[&str],
) -> Result<()> {
    let missing: Vec<&str> = required
        .iter()
        .copied()
        .filter(|column| !columns.contains(*column))
        .collect();
    if !missing.is_empty() {
        bail!(
            "{} table `{table}` lacks column(s) {}; this tsift reads {SCHEMA_ANSWER_LEDGER_EDGES_V1} and {SCHEMA_DECISION_GRAPH_V2} caches only",
            state_db.display(),
            missing.join(", ")
        );
    }
    Ok(())
}

/// An optional column as a SELECT expression (`NULL` when absent).
fn optional_column(columns: &BTreeSet<String>, column: &str) -> String {
    if columns.contains(column) {
        column.to_string()
    } else {
        "NULL".to_string()
    }
}

fn open_read_only(state_db: &Path) -> Result<Connection> {
    if !state_db.is_file() {
        bail!(
            "no agent-doc state database at {} (agent-doc caches decision graphs there once a session document has been answered)",
            state_db.display()
        );
    }
    let conn = Connection::open_with_flags(
        state_db,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .with_context(|| format!("opening {} read-only", state_db.display()))?;
    conn.busy_timeout(BUSY_TIMEOUT)?;
    conn.execute_batch("PRAGMA query_only = ON")?;
    Ok(conn)
}

fn read_cache(state_db: &Path) -> Result<Cache> {
    let conn = open_read_only(state_db)?;
    // One deferred read transaction: nodes, edges and paths come from the same
    // snapshot even while the controller keeps writing.
    let tx = conn.unchecked_transaction()?;
    let Some(edge_columns) = table_columns(&tx, EDGE_TABLE)? else {
        bail!(
            "{} has no `{EDGE_TABLE}` table; it is not an agent-doc state database with a cached decision graph",
            state_db.display()
        );
    };
    require_columns(state_db, EDGE_TABLE, &edge_columns, &EDGE_REQUIRED)?;
    let node_columns = table_columns(&tx, NODE_TABLE)?;
    if let Some(columns) = &node_columns {
        require_columns(state_db, NODE_TABLE, columns, &NODE_REQUIRED)?;
    }
    let mut cache = Cache {
        schema: if node_columns.is_some() {
            SCHEMA_DECISION_GRAPH_V2
        } else {
            SCHEMA_ANSWER_LEDGER_EDGES_V1
        },
        ..Cache::default()
    };

    let sql = format!(
        "SELECT document_id, from_id, kind, to_id, {}, {} FROM {EDGE_TABLE}
         ORDER BY document_id, from_id, kind, to_id",
        optional_column(&edge_columns, "source"),
        optional_column(&edge_columns, "clause"),
    );
    let mut statement = tx.prepare(&sql)?;
    let rows = statement.query_map([], |row| {
        Ok(CachedEdge {
            document_id: row.get(0)?,
            from: row.get(1)?,
            kind: row.get(2)?,
            to: row.get(3)?,
            source: row.get(4)?,
            clause: row.get(5)?,
        })
    })?;
    for row in rows {
        cache.edges.push(row?);
    }
    drop(statement);

    if let Some(columns) = &node_columns {
        let sql = format!(
            "SELECT document_id, node_id, kind, label, {}, {}, {} FROM {NODE_TABLE}
             ORDER BY document_id, node_id",
            optional_column(columns, "open"),
            optional_column(columns, "location"),
            optional_column(columns, "first_seen_at"),
        );
        let mut statement = tx.prepare(&sql)?;
        let rows = statement.query_map([], |row| {
            Ok(CachedNode {
                document_id: row.get(0)?,
                id: row.get(1)?,
                kind: row.get(2)?,
                label: row.get(3)?,
                open: row.get::<_, Option<i64>>(4)?.map(|value| value != 0),
                location: row.get(5)?,
                first_seen_at: row.get(6)?,
            })
        })?;
        for row in rows {
            cache.nodes.push(row?);
        }
    }

    if let Some(columns) = table_columns(&tx, DOCUMENT_TABLE)?
        && columns.contains("document_id")
        && columns.contains("canonical_path")
    {
        let mut statement =
            tx.prepare("SELECT document_id, canonical_path FROM documents ORDER BY document_id")?;
        let rows = statement.query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })?;
        for row in rows {
            let (id, path) = row?;
            cache.paths.insert(id, path);
        }
    }
    drop(tx);
    Ok(cache)
}

/// Node kinds whose ids mean the same thing in every document.
fn shared_rank(kind: &str) -> Option<u8> {
    match kind {
        "question" => Some(3),
        "evidence" => Some(2),
        "item" => Some(1),
        _ => None,
    }
}

/// The kind an edge's typed contract implies for an endpoint without a node
/// row (every v1 endpoint; a v2 endpoint the node table missed).
fn implied_kinds(edge_kind: &str) -> (&'static str, &'static str) {
    match edge_kind {
        "answers" => ("revision", "decision"),
        "restates" | "amends" | "supersedes" => ("revision", "revision"),
        "copy_of" => ("question", "decision"),
        "evidenced_by" => ("item", "evidence"),
        _ => ("item", "item"),
    }
}

/// Rank an implied kind: anything specific beats the `item` placeholder.
fn implied_rank(kind: &str) -> u8 {
    u8::from(kind != "item")
}

fn graph_kind(kind: &str) -> String {
    let sanitized: String = kind
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() {
                c.to_ascii_lowercase()
            } else {
                '_'
            }
        })
        .collect();
    format!("{KIND_PREFIX}{sanitized}")
}

fn local_id(document_id: &str, id: &str, kind: &str) -> String {
    if shared_rank(kind).is_some() {
        format!("ref:{id}")
    } else {
        format!("{document_id}/{id}")
    }
}

fn document_local_id(document_id: &str) -> String {
    format!("document:{document_id}")
}

/// Root-relative display path of a document, when agent-doc recorded one.
fn document_path(root: &Path, canonical: Option<&String>) -> (Option<String>, bool) {
    let Some(canonical) = canonical else {
        return (None, false);
    };
    let root = fs::canonicalize(root).unwrap_or_else(|_| root.to_path_buf());
    match Path::new(canonical).strip_prefix(&root) {
        Ok(relative) => (Some(relative.to_string_lossy().replace('\\', "/")), true),
        Err(_) => (Some(canonical.clone()), false),
    }
}

fn source_rank(source: &str) -> u8 {
    match source {
        "operator" => 3,
        "derived" => 2,
        _ => 1,
    }
}

/// Lower the decision graphs cached in `state_db` into one edge file.
pub(crate) fn lower_agent_doc_decisions(state_db: &Path, root: &Path) -> Result<EdgeFile> {
    let mut cache = read_cache(state_db)?;
    let named_by_hash = name_documents_by_path_hash(&mut cache, root);
    let mut file = lower_cache(&cache, root)?;
    if named_by_hash > 0 {
        file.properties.insert(
            "documents_named_by_path_hash".to_string(),
            Value::from(named_by_hash),
        );
    }
    Ok(file)
}

/// agent-doc's document id: SHA-256 hex of the canonical path string
/// (agent-doc-hash `document_id_for_path`).
fn document_id_for_canonical(path: &Path) -> String {
    use sha2::{Digest, Sha256};
    format!("{:x}", Sha256::digest(path.to_string_lossy().as_bytes()))
}

/// Name the cached documents agent-doc's `documents` table does not: a
/// non-hash id is agent-doc's own fallback (the display path itself), and a
/// hash id is matched by hashing the canonical path of each Markdown file
/// under `root` (gitignore-aware, bounded). Returns how many were named by
/// hash.
fn name_documents_by_path_hash(cache: &mut Cache, root: &Path) -> usize {
    let mut unnamed: BTreeSet<String> = cache
        .edges
        .iter()
        .map(|edge| edge.document_id.clone())
        .chain(cache.nodes.iter().map(|node| node.document_id.clone()))
        .filter(|id| !cache.paths.contains_key(id))
        .collect();
    let is_hash = |id: &str| id.len() == 64 && id.chars().all(|c| c.is_ascii_hexdigit());
    for id in unnamed.clone() {
        if !is_hash(&id) && Path::new(&id).is_absolute() {
            cache.paths.insert(id.clone(), id.clone());
            unnamed.remove(&id);
        }
    }
    unnamed.retain(|id| is_hash(id));
    if unnamed.is_empty() {
        return 0;
    }
    let mut named = 0;
    for entry in ignore::WalkBuilder::new(root)
        .build()
        .take(DOCUMENT_WALK_LIMIT)
        .flatten()
    {
        let path = entry.path();
        if !path
            .extension()
            .is_some_and(|extension| extension.eq_ignore_ascii_case("md"))
        {
            continue;
        }
        let Ok(canonical) = fs::canonicalize(path) else {
            continue;
        };
        let id = document_id_for_canonical(&canonical);
        if unnamed.remove(&id) {
            cache
                .paths
                .insert(id, canonical.to_string_lossy().to_string());
            named += 1;
            if unnamed.is_empty() {
                break;
            }
        }
    }
    named
}

fn lower_cache(cache: &Cache, root: &Path) -> Result<EdgeFile> {
    // Each (document, id) gets one kind: its node row, else what its edges imply.
    let mut kinds: BTreeMap<(String, String), (String, u8)> = BTreeMap::new();
    for edge in &cache.edges {
        let (from_kind, to_kind) = implied_kinds(&edge.kind);
        for (id, kind) in [(&edge.from, from_kind), (&edge.to, to_kind)] {
            let key = (edge.document_id.clone(), id.clone());
            let rank = implied_rank(kind);
            let entry = kinds.entry(key).or_insert_with(|| (kind.to_string(), rank));
            if rank > entry.1 {
                *entry = (kind.to_string(), rank);
            }
        }
    }
    let mut rows: BTreeMap<(String, String), &CachedNode> = BTreeMap::new();
    for node in &cache.nodes {
        let key = (node.document_id.clone(), node.id.clone());
        kinds.insert(key.clone(), (node.kind.clone(), u8::MAX));
        rows.insert(key, node);
    }

    // An item reference to an id exactly one other document owns as a
    // document-local node (a decision, a revision) is that node.
    let mut owners: BTreeMap<&str, BTreeSet<&str>> = BTreeMap::new();
    for ((document_id, id), (kind, _)) in &kinds {
        if shared_rank(kind).is_none() {
            owners.entry(id).or_default().insert(document_id);
        }
    }
    let mut redirect: BTreeMap<(String, String), (String, String)> = BTreeMap::new();
    for ((document_id, id), (kind, _)) in &kinds {
        if kind != "item" {
            continue;
        }
        if let Some(owner) = owners.get(id.as_str())
            && owner.len() == 1
            && let Some(owner) = owner.iter().next()
            && *owner != document_id
        {
            redirect.insert(
                (document_id.clone(), id.clone()),
                (owner.to_string(), format!("{owner}/{id}")),
            );
        }
    }

    let documents: BTreeSet<String> = kinds.keys().map(|(document, _)| document.clone()).collect();
    let mut nodes: Vec<EdgeFileNode> = Vec::new();
    let mut edges: BTreeMap<(String, String, String), BTreeMap<String, Value>> = BTreeMap::new();
    let mut edge_documents: BTreeMap<(String, String, String), BTreeSet<String>> = BTreeMap::new();
    let mut display: BTreeMap<String, String> = BTreeMap::new();

    for document_id in &documents {
        let (path, under_root) = document_path(root, cache.paths.get(document_id));
        let label = path
            .clone()
            .unwrap_or_else(|| format!("document:{}", &document_id[..document_id.len().min(12)]));
        display.insert(document_id.clone(), label.clone());
        let mut properties = BTreeMap::new();
        properties.insert("document_id".to_string(), Value::from(document_id.as_str()));
        if let Some(path) = &path {
            properties.insert("path".to_string(), Value::from(path.as_str()));
        }
        nodes.push(EdgeFileNode {
            id: document_local_id(document_id),
            kind: Some(DOCUMENT_KIND.to_string()),
            label: Some(label),
            join: under_root.then(|| {
                JoinSpec::Spec(JoinFields {
                    path: path.clone(),
                    ..JoinFields::default()
                })
            }),
            link: None,
            properties,
        });
    }

    // Shared nodes: one per id across documents, the strongest kind wins.
    struct Shared {
        kind: String,
        rank: u8,
        label: String,
        open: Option<bool>,
        documents: BTreeSet<String>,
    }
    let mut shared: BTreeMap<String, Shared> = BTreeMap::new();
    for ((document_id, id), (kind, _)) in &kinds {
        let row = rows.get(&(document_id.clone(), id.clone())).copied();
        let label = row.map(|row| row.label.clone()).unwrap_or_default();
        let open = row.and_then(|row| row.open);
        let local = local_id(document_id, id, kind);
        let document = display
            .get(document_id)
            .cloned()
            .unwrap_or_else(|| document_id.clone());
        if let Some((owner, target)) = redirect.get(&(document_id.clone(), id.clone())) {
            let mut properties = BTreeMap::new();
            properties.insert("document".to_string(), Value::from(document));
            properties.insert(
                "owner_document".to_string(),
                Value::from(display.get(owner).cloned().unwrap_or_else(|| owner.clone())),
            );
            edges.insert(
                (
                    target.clone(),
                    "mentioned_in".to_string(),
                    document_local_id(document_id),
                ),
                properties,
            );
            continue;
        }
        if let Some(rank) = shared_rank(kind) {
            let entry = shared.entry(id.clone()).or_insert_with(|| Shared {
                kind: kind.clone(),
                rank,
                label: String::new(),
                open: None,
                documents: BTreeSet::new(),
            });
            if rank > entry.rank {
                entry.kind = kind.clone();
                entry.rank = rank;
            }
            if entry.label.is_empty() {
                entry.label = label;
            }
            entry.open = match (entry.open, open) {
                (Some(true), _) | (_, Some(true)) => Some(true),
                (Some(false), _) | (_, Some(false)) => Some(false),
                _ => None,
            };
            entry.documents.insert(document_id.clone());
            let mut properties = BTreeMap::new();
            properties.insert("document".to_string(), Value::from(document));
            if let Some(open) = open {
                properties.insert("open".to_string(), Value::from(open.to_string()));
            }
            if let Some(location) = row.and_then(|row| row.location.clone()) {
                properties.insert("location".to_string(), Value::from(location));
            }
            if let Some(first_seen_at) = row.and_then(|row| row.first_seen_at) {
                properties.insert("first_seen_at".to_string(), Value::from(first_seen_at));
            }
            edges.insert(
                (
                    local,
                    "mentioned_in".to_string(),
                    document_local_id(document_id),
                ),
                properties,
            );
            continue;
        }
        let mut properties = BTreeMap::new();
        properties.insert("ref_id".to_string(), Value::from(id.as_str()));
        properties.insert("document_id".to_string(), Value::from(document_id.as_str()));
        properties.insert("document".to_string(), Value::from(document));
        if let Some(open) = open {
            properties.insert("open".to_string(), Value::from(open.to_string()));
        }
        if let Some(row) = row {
            if let Some(location) = &row.location {
                properties.insert("location".to_string(), Value::from(location.as_str()));
            }
            if let Some(first_seen_at) = row.first_seen_at {
                properties.insert("first_seen_at".to_string(), Value::from(first_seen_at));
            }
        }
        nodes.push(EdgeFileNode {
            id: local.clone(),
            kind: Some(graph_kind(kind)),
            label: Some(if label.is_empty() { id.clone() } else { label }),
            join: None,
            link: None,
            properties,
        });
        edges.insert(
            (
                local,
                "in_document".to_string(),
                document_local_id(document_id),
            ),
            BTreeMap::new(),
        );
    }
    for (id, entry) in &shared {
        let mut properties = BTreeMap::new();
        properties.insert("ref_id".to_string(), Value::from(id.as_str()));
        properties.insert("documents".to_string(), Value::from(entry.documents.len()));
        if let Some(open) = entry.open {
            properties.insert("open".to_string(), Value::from(open.to_string()));
        }
        nodes.push(EdgeFileNode {
            id: format!("ref:{id}"),
            kind: Some(graph_kind(&entry.kind)),
            label: Some(if entry.label.is_empty() {
                id.clone()
            } else {
                entry.label.clone()
            }),
            join: None,
            // A shared id is the same id in every document, so each indexed
            // backlog node with that id links to it and `graph-db evidence
            // <id>` reaches the decisions about the item
            // (`#decisionitembacklogjoin`). The link also covers an item one
            // document outranked to a question; an id no backlog carries
            // (`commit:<sha>`, `r-<hex>`) links nothing.
            link: Some(LinkSpec {
                edge: BACKLOG_ITEM_EDGE.to_string(),
                kinds: vec![BACKLOG_KIND.to_string()],
                ref_id: id.clone(),
            }),
            properties,
        });
    }

    // Decision-graph edges, merged across documents where both ends are shared.
    let mut skipped_rows = 0_usize;
    for edge in &cache.edges {
        if edge.from.trim().is_empty() || edge.to.trim().is_empty() || edge.kind.trim().is_empty() {
            skipped_rows += 1;
            continue;
        }
        let kind_of = |id: &str| {
            kinds
                .get(&(edge.document_id.clone(), id.to_string()))
                .map(|(kind, _)| kind.as_str())
                .unwrap_or("item")
        };
        let resolve = |id: &str| {
            redirect
                .get(&(edge.document_id.clone(), id.to_string()))
                .map(|(_, target)| target.clone())
                .unwrap_or_else(|| local_id(&edge.document_id, id, kind_of(id)))
        };
        let from = resolve(&edge.from);
        let to = resolve(&edge.to);
        let key = (from, edge.kind.clone(), to);
        edge_documents
            .entry(key.clone())
            .or_default()
            .insert(edge.document_id.clone());
        let properties = edges.entry(key).or_default();
        let source = edge.source.clone().unwrap_or_else(|| "derived".to_string());
        let stronger = properties
            .get("edge_source")
            .and_then(Value::as_str)
            .is_none_or(|current| source_rank(&source) > source_rank(current));
        if stronger {
            properties.insert("edge_source".to_string(), Value::from(source));
        }
        if let Some(clause) = &edge.clause
            && !properties.contains_key("clause")
        {
            properties.insert("clause".to_string(), Value::from(clause.as_str()));
        }
    }
    for (key, document_ids) in &edge_documents {
        let names: Vec<String> = document_ids
            .iter()
            .map(|id| display.get(id).cloned().unwrap_or_else(|| id.clone()))
            .collect();
        if let Some(properties) = edges.get_mut(key) {
            properties.insert("documents".to_string(), Value::from(names.join(", ")));
        }
    }

    let mut properties = BTreeMap::new();
    properties.insert("agent_doc_schema".to_string(), Value::from(cache.schema));
    properties.insert(
        "agent_doc_documents".to_string(),
        Value::from(documents.len()),
    );
    properties.insert("cache_nodes".to_string(), Value::from(cache.nodes.len()));
    properties.insert("cache_edges".to_string(), Value::from(cache.edges.len()));
    if skipped_rows > 0 {
        properties.insert("skipped_rows".to_string(), Value::from(skipped_rows));
    }
    Ok(EdgeFile {
        format: None,
        source: Some(AGENT_DOC_DECISIONS_SOURCE.to_string()),
        imported_from: None,
        adapter: Some(AGENT_DOC_DECISIONS_ADAPTER.to_string()),
        properties,
        nodes,
        edges: edges
            .into_iter()
            .map(|((from, kind, to), properties)| EdgeFileEdge {
                from,
                to,
                kind,
                properties,
            })
            .collect(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const DOC_A: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    const DOC_B: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";

    /// `(document, from, kind, to, source, clause)`.
    type CacheEdgeRow = (
        &'static str,
        &'static str,
        &'static str,
        &'static str,
        &'static str,
        Option<&'static str>,
    );

    /// agent-doc's `#decisiongraph` cache DDL (agent-doc-sqlite
    /// `answer_ledger_index::ensure_schema`) plus its `documents` table.
    fn v2_db(path: &Path, root: &Path) {
        let conn = Connection::open(path).unwrap();
        conn.execute_batch(
            r#"
            PRAGMA journal_mode = WAL;
            CREATE TABLE answer_ledger_edges (
                document_id TEXT NOT NULL, from_id TEXT NOT NULL, kind TEXT NOT NULL,
                to_id TEXT NOT NULL, recorded_at INTEGER NOT NULL,
                source TEXT NOT NULL DEFAULT 'derived', clause TEXT,
                PRIMARY KEY (document_id, from_id, kind, to_id));
            CREATE TABLE answer_ledger_nodes (
                document_id TEXT NOT NULL, node_id TEXT NOT NULL, kind TEXT NOT NULL,
                label TEXT NOT NULL, open INTEGER, location TEXT, first_seen_at INTEGER,
                recorded_at INTEGER NOT NULL, PRIMARY KEY (document_id, node_id));
            CREATE TABLE documents (document_id TEXT PRIMARY KEY, canonical_path TEXT NOT NULL);
            "#,
        )
        .unwrap();
        let root = fs::canonicalize(root).unwrap();
        for (doc, name) in [(DOC_A, "a.md"), (DOC_B, "b.md")] {
            conn.execute(
                "INSERT INTO documents VALUES (?1, ?2)",
                [doc, root.join(name).to_string_lossy().as_ref()],
            )
            .unwrap();
        }
        let nodes: [(&str, &str, &str, &str, Option<i64>); 8] = [
            (DOC_A, "dec-1", "decision", "Ship X?", Some(0)),
            (DOC_A, "dec-9", "decision", "Use flags?", Some(0)),
            (DOC_A, "dec-1@r1", "revision", "yes", None),
            (DOC_A, "dec-1@r2", "revision", "yes, behind a flag", None),
            (DOC_A, "shipx", "item", "ship x", Some(1)),
            (DOC_B, "dec-1", "decision", "Unrelated decision", Some(1)),
            (DOC_B, "shipx", "question", "Ship X when?", Some(0)),
            (DOC_B, "commit:abc1234", "evidence", "", None),
        ];
        for (doc, id, kind, label, open) in nodes {
            conn.execute(
                "INSERT INTO answer_ledger_nodes VALUES (?1, ?2, ?3, ?4, ?5, NULL, 7, 1)",
                rusqlite::params![doc, id, kind, label, open],
            )
            .unwrap();
        }
        let edges: [CacheEdgeRow; 6] = [
            (DOC_A, "dec-1@r1", "answers", "dec-1", "derived", None),
            (
                DOC_A,
                "dec-1@r2",
                "amends",
                "dec-1@r1",
                "operator",
                Some("behind a flag"),
            ),
            (DOC_A, "dec-1", "applies_to", "shipx", "derived", None),
            (DOC_B, "dec-1", "applies_to", "shipx", "derived", None),
            (
                DOC_B,
                "dec-1",
                "evidenced_by",
                "commit:abc1234",
                "derived",
                None,
            ),
            (DOC_B, "dec-1", "depends_on", "dec-9", "operator", None),
        ];
        for (doc, from, kind, to, source, clause) in edges {
            conn.execute(
                "INSERT INTO answer_ledger_edges VALUES (?1, ?2, ?3, ?4, 1, ?5, ?6)",
                rusqlite::params![doc, from, kind, to, source, clause],
            )
            .unwrap();
        }
    }

    fn node<'a>(file: &'a EdgeFile, id: &str) -> &'a EdgeFileNode {
        file.nodes
            .iter()
            .find(|node| node.id == id)
            .unwrap_or_else(|| panic!("node {id} missing: {:#?}", file.nodes))
    }

    fn edge<'a>(file: &'a EdgeFile, from: &str, kind: &str, to: &str) -> &'a EdgeFileEdge {
        file.edges
            .iter()
            .find(|edge| edge.from == from && edge.kind == kind && edge.to == to)
            .unwrap_or_else(|| panic!("edge {from} {kind} {to} missing: {:#?}", file.edges))
    }

    #[test]
    fn v2_cache_lowers_document_local_decisions_and_shared_items() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("state.db");
        v2_db(&db, dir.path());
        let before = fs::read(&db).unwrap();
        let file = lower_agent_doc_decisions(&db, dir.path()).unwrap();
        assert_eq!(fs::read(&db).unwrap(), before, "the reader must not write");
        assert_eq!(
            file.properties["agent_doc_schema"],
            SCHEMA_DECISION_GRAPH_V2
        );
        assert_eq!(file.adapter.as_deref(), Some(AGENT_DOC_DECISIONS_ADAPTER));

        // Same decision id in two documents: two document-local nodes.
        let a = format!("{DOC_A}/dec-1");
        let b = format!("{DOC_B}/dec-1");
        assert_eq!(node(&file, &a).kind.as_deref(), Some("agent_doc_decision"));
        assert_eq!(node(&file, &a).label.as_deref(), Some("Ship X?"));
        assert_eq!(node(&file, &b).label.as_deref(), Some("Unrelated decision"));
        assert_eq!(node(&file, &a).properties["document"], "a.md");

        // One shared node for `shipx`; the question outranks the item.
        let shipx = node(&file, "ref:shipx");
        assert_eq!(shipx.kind.as_deref(), Some("agent_doc_question"));
        // Shared ids link to the backlog nodes with the same id; document-local
        // decisions do not (`#decisionitembacklogjoin`).
        assert_eq!(
            shipx.link,
            Some(LinkSpec {
                edge: BACKLOG_ITEM_EDGE.to_string(),
                kinds: vec!["backlog".to_string()],
                ref_id: "shipx".to_string(),
            })
        );
        assert!(node(&file, &a).link.is_none());
        assert_eq!(shipx.properties["documents"], 2);
        assert_eq!(shipx.properties["open"], "true");
        edge(&file, &a, "applies_to", "ref:shipx");
        edge(&file, &b, "applies_to", "ref:shipx");
        let mention = edge(
            &file,
            "ref:shipx",
            "mentioned_in",
            &format!("document:{DOC_B}"),
        );
        assert_eq!(mention.properties["open"], "false");

        // Provenance and amended clause survive.
        let amends = edge(
            &file,
            &format!("{DOC_A}/dec-1@r2"),
            "amends",
            &format!("{DOC_A}/dec-1@r1"),
        );
        assert_eq!(amends.properties["edge_source"], "operator");
        assert_eq!(amends.properties["clause"], "behind a flag");

        // Documents join their indexed file by root-relative path.
        let document = node(&file, &format!("document:{DOC_A}"));
        assert_eq!(document.label.as_deref(), Some("a.md"));
        assert!(matches!(
            &document.join,
            Some(JoinSpec::Spec(JoinFields { path: Some(path), .. })) if path == "a.md"
        ));
        assert_eq!(
            node(&file, "ref:commit:abc1234").kind.as_deref(),
            Some("agent_doc_evidence")
        );

        // B depends on a decision only A owns: the reference crosses documents.
        let dec9 = format!("{DOC_A}/dec-9");
        let depends = edge(&file, &b, "depends_on", &dec9);
        assert_eq!(depends.properties["edge_source"], "operator");
        assert!(file.nodes.iter().all(|node| node.id != "ref:dec-9"));
        let mention = edge(&file, &dec9, "mentioned_in", &format!("document:{DOC_B}"));
        assert_eq!(mention.properties["owner_document"], "a.md");
    }

    #[test]
    fn v1_cache_infers_kinds_from_typed_edges() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("state.db");
        let conn = Connection::open(&db).unwrap();
        conn.execute_batch(
            "CREATE TABLE answer_ledger_edges (document_id TEXT NOT NULL, from_id TEXT NOT NULL,
                kind TEXT NOT NULL, to_id TEXT NOT NULL, recorded_at INTEGER NOT NULL);
             INSERT INTO answer_ledger_edges VALUES
                ('d', 'dec-9@x', 'answers', 'dec-9', 1),
                ('d', 'req', 'applies_to', 'work', 1);",
        )
        .unwrap();
        drop(conn);
        let file = lower_agent_doc_decisions(&db, dir.path()).unwrap();
        assert_eq!(
            file.properties["agent_doc_schema"],
            SCHEMA_ANSWER_LEDGER_EDGES_V1
        );
        assert_eq!(
            node(&file, "d/dec-9").kind.as_deref(),
            Some("agent_doc_decision")
        );
        assert_eq!(
            node(&file, "d/dec-9@x").kind.as_deref(),
            Some("agent_doc_revision")
        );
        assert_eq!(
            node(&file, "ref:work").kind.as_deref(),
            Some("agent_doc_item")
        );
        let answers = edge(&file, "d/dec-9@x", "answers", "d/dec-9");
        assert_eq!(answers.properties["edge_source"], "derived");
        // No `documents` table: the document is named by its id prefix, unjoined.
        let document = node(&file, "document:d");
        assert_eq!(document.label.as_deref(), Some("document:d"));
        assert!(document.join.is_none());
    }

    #[test]
    fn documents_missing_from_the_documents_table_are_named_by_path_hash() {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir_all(dir.path().join("tasks")).unwrap();
        let doc = dir.path().join("tasks/plan.md");
        fs::write(&doc, "# plan\n").unwrap();
        let id = document_id_for_canonical(&fs::canonicalize(&doc).unwrap());
        let db = dir.path().join("state.db");
        let conn = Connection::open(&db).unwrap();
        conn.execute_batch(
            "CREATE TABLE answer_ledger_edges (document_id TEXT NOT NULL, from_id TEXT NOT NULL,
                kind TEXT NOT NULL, to_id TEXT NOT NULL, recorded_at INTEGER NOT NULL);",
        )
        .unwrap();
        conn.execute(
            "INSERT INTO answer_ledger_edges VALUES (?1, 'dec-1@a', 'answers', 'dec-1', 1)",
            [&id],
        )
        .unwrap();
        drop(conn);
        let file = lower_agent_doc_decisions(&db, dir.path()).unwrap();
        let document = node(&file, &format!("document:{id}"));
        assert_eq!(document.label.as_deref(), Some("tasks/plan.md"));
        assert_eq!(file.properties["documents_named_by_path_hash"], 1);
    }

    #[test]
    fn missing_or_unrecognized_cache_is_refused_without_creating_anything() {
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("state.db");
        let err = lower_agent_doc_decisions(&missing, dir.path()).unwrap_err();
        assert!(
            err.to_string().contains("no agent-doc state database"),
            "{err:#}"
        );
        assert!(
            !missing.exists(),
            "a read-only reader must not create the file"
        );

        let conn = Connection::open(&missing).unwrap();
        conn.execute_batch("CREATE TABLE other (x INTEGER);")
            .unwrap();
        drop(conn);
        let err = lower_agent_doc_decisions(&missing, dir.path()).unwrap_err();
        assert!(
            err.to_string().contains("no `answer_ledger_edges` table"),
            "{err:#}"
        );

        let conn = Connection::open(&missing).unwrap();
        conn.execute_batch("CREATE TABLE answer_ledger_edges (document_id TEXT, kind TEXT);")
            .unwrap();
        drop(conn);
        let err = lower_agent_doc_decisions(&missing, dir.path()).unwrap_err();
        assert!(err.to_string().contains("from_id, to_id"), "{err:#}");
        assert!(is_sqlite_file(&missing));
        assert!(!is_sqlite_file(dir.path()));
    }
}
