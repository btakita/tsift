use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;
use std::path::Path;
use std::process::Command;
use tsift_graph as graph;
use tsift_graph::lang::Lang;
use tsift_quality::lint;
use tsift_summarize::summarize::{self, SummaryDb};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DiffDigestMode {
    WorkingTree,
    Cached,
    Revision,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct DiffDigestOptions<'a> {
    pub cached: bool,
    pub revision: Option<&'a str>,
    /// Cap how many changed files get a full tree-sitter parse for symbols and
    /// call-edges. `None` parses every changed file (the historical default).
    /// `Some(N)` parses the first `N` files in sort order and emits cheap
    /// path-only entries for the rest (`touched_symbols`, `current_summaries`,
    /// `added_call_edges`, `removed_call_edges` all empty, marked with a
    /// `parse_deferred_by_budget` warning). The aggregate `symbols_touched`,
    /// `call_edges_added`, and `call_edges_removed` reflect only the parsed
    /// subset; `files_changed` always counts every changed path. Used by
    /// `context-pack` to avoid parsing every working-tree change when the
    /// preview budget only takes the first N anyway (`#gdbprephot`).
    pub max_parsed_files: Option<usize>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DiffDigestFileStatus {
    Added,
    Modified,
    Deleted,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DiffDigestSummaryState {
    Current,
    Stale,
    Missing,
    Unavailable,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DiffDigestSummarySnippet {
    pub symbol: String,
    pub summary: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DiffDigestFile {
    pub path: String,
    pub status: DiffDigestFileStatus,
    pub touched_symbols: Vec<String>,
    /// Structural summary for document files (`#docsym`). Markdown headings are
    /// navigation structure, not symbols — they have no callers or callees — so
    /// they are reported here instead of padding `touched_symbols` with heading
    /// text and clipped prose. Always empty for code files.
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub touched_headings: Vec<String>,
    pub summary_state: DiffDigestSummaryState,
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub current_summaries: Vec<DiffDigestSummarySnippet>,
    pub added_call_edges: Vec<String>,
    pub removed_call_edges: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub warnings: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DiffDigestReport {
    pub root: String,
    pub mode: DiffDigestMode,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub revision: Option<String>,
    pub files_changed: usize,
    pub files_with_current_summaries: usize,
    pub symbols_touched: usize,
    /// Document headings touched across the diff (`#docsym`). Counted apart from
    /// `symbols_touched` so `9 files changed, 40 touched symbols` can no longer
    /// mean "a README grew some headings".
    #[serde(default)]
    pub headings_touched: usize,
    pub call_edges_added: usize,
    pub call_edges_removed: usize,
    pub files: Vec<DiffDigestFile>,
}

#[derive(Debug, Default)]
struct ParsedSnapshot {
    symbol_names: Vec<String>,
    heading_names: Vec<String>,
    edges: BTreeSet<String>,
    warnings: Vec<String>,
}

#[derive(Debug, Clone)]
enum SnapshotSource {
    WorkingTree,
    Index,
    GitRef(String),
}

#[derive(Debug, Clone)]
struct RevisionBounds {
    base: Option<String>,
    target: String,
}

pub fn compute(path: &Path, options: DiffDigestOptions<'_>) -> Result<DiffDigestReport> {
    let root = lint::resolve_project_root_or_canonical_path(path)?;
    let mode = resolve_mode(&root, options)?;
    let changed = collect_changed_files(&root, &mode)?;
    let summary_db = open_summary_db_if_present(&root)?;

    // First pass: collect (path, status, existing) tuples after artifact filter.
    // We need the deterministic sort order before deciding which files actually
    // get an expensive tree-sitter parse so that `max_parsed_files` always
    // selects the first N in canonical sort order.
    let mut entries: Vec<(std::path::PathBuf, DiffDigestFileStatus, bool)> = Vec::new();
    for file_path in changed.existing {
        if is_internal_tsift_artifact(&root, &file_path) {
            continue;
        }
        entries.push((file_path, DiffDigestFileStatus::Modified, true));
    }
    for file_path in changed.deleted {
        if is_internal_tsift_artifact(&root, &file_path) {
            continue;
        }
        entries.push((file_path, DiffDigestFileStatus::Deleted, false));
    }
    entries.sort_by(|left, right| {
        relative_git_path(&root, &left.0).cmp(&relative_git_path(&root, &right.0))
    });

    let parse_budget = options.max_parsed_files;
    let mut parsed_count = 0usize;

    let mut files = Vec::with_capacity(entries.len());
    for (file_path, mut status, existing) in entries {
        let parse_this = parse_budget.is_none_or(|n| parsed_count < n);
        if !parse_this {
            // #gdbprephot: skip the per-file `git show HEAD:path` /
            // `git ls-files --stage` lookups required to determine
            // Added vs Modified for deferred entries. The preview window
            // never includes them, so the Modified-vs-Added distinction
            // is irrelevant once we've decided not to parse.
            files.push(build_parse_deferred_diff_file(&root, &file_path, status));
            continue;
        }
        if existing {
            let previous = load_previous_bytes(&root, &mode, &file_path)?;
            if previous.is_none() {
                status = DiffDigestFileStatus::Added;
            }
            let (current, warnings) = load_current_bytes(&root, &mode, &file_path);
            files.push(build_diff_file(
                &root,
                summary_db.as_ref(),
                &file_path,
                status,
                previous.as_deref(),
                current.as_deref(),
                warnings,
            )?);
            parsed_count += 1;
        } else {
            files.push(build_deleted_diff_file(
                &root,
                &mode,
                summary_db.as_ref(),
                &file_path,
            )?);
            parsed_count += 1;
        }
    }

    let files_with_current_summaries = files
        .iter()
        .filter(|file| file.summary_state == DiffDigestSummaryState::Current)
        .count();
    let symbols_touched = files.iter().map(|file| file.touched_symbols.len()).sum();
    let headings_touched = files.iter().map(|file| file.touched_headings.len()).sum();
    let call_edges_added = files.iter().map(|file| file.added_call_edges.len()).sum();
    let call_edges_removed = files.iter().map(|file| file.removed_call_edges.len()).sum();

    Ok(DiffDigestReport {
        root: root.display().to_string(),
        mode: mode.report_mode(),
        revision: mode.report_revision(),
        files_changed: files.len(),
        files_with_current_summaries,
        symbols_touched,
        headings_touched,
        call_edges_added,
        call_edges_removed,
        files,
    })
}

fn open_summary_db_if_present(root: &Path) -> Result<Option<SummaryDb>> {
    let db_path = root.join(".tsift/summaries.db");
    if !db_path.exists() {
        return Ok(None);
    }
    Ok(Some(SummaryDb::open_read_only_with_recovery(&db_path)?.db))
}

fn build_diff_file(
    root: &Path,
    summary_db: Option<&SummaryDb>,
    file_path: &Path,
    status: DiffDigestFileStatus,
    previous_bytes: Option<&[u8]>,
    current_bytes: Option<&[u8]>,
    mut warnings: Vec<String>,
) -> Result<DiffDigestFile> {
    let rel_path = relative_git_path(root, file_path);

    let previous = previous_bytes
        .map(|bytes| parse_snapshot(file_path, bytes))
        .unwrap_or_default();
    let current = current_bytes
        .map(|bytes| parse_snapshot(file_path, bytes))
        .unwrap_or_default();
    warnings.extend(previous.warnings);
    warnings.extend(current.warnings);

    let touched_symbols = merge_symbol_names(&previous.symbol_names, &current.symbol_names);
    let touched_headings = merge_symbol_names(&previous.heading_names, &current.heading_names);
    let added_call_edges = diff_edges(&current.edges, &previous.edges);
    let removed_call_edges = diff_edges(&previous.edges, &current.edges);
    let content_hash = current_bytes
        .map(summarize::content_hash)
        .or_else(|| previous_bytes.map(summarize::content_hash));
    let (summary_state, current_summaries) = collect_current_summaries(
        summary_db,
        &rel_path,
        content_hash.as_deref(),
        &touched_symbols,
    )?;

    Ok(DiffDigestFile {
        path: rel_path,
        status,
        touched_symbols,
        touched_headings,
        summary_state,
        current_summaries,
        added_call_edges,
        removed_call_edges,
        warnings,
    })
}

/// Cheap path-only `DiffDigestFile` for changed paths beyond the caller's
/// `max_parsed_files` budget. Skips tree-sitter parsing, content-hash, and
/// summary-cache lookups so a context-pack preview does not pay full
/// per-file parse cost on changed files it would never include in the
/// truncated preview window. Used by `#gdbprephot`.
fn build_parse_deferred_diff_file(
    root: &Path,
    file_path: &Path,
    status: DiffDigestFileStatus,
) -> DiffDigestFile {
    DiffDigestFile {
        path: relative_git_path(root, file_path),
        status,
        touched_symbols: Vec::new(),
        touched_headings: Vec::new(),
        summary_state: DiffDigestSummaryState::Unavailable,
        current_summaries: Vec::new(),
        added_call_edges: Vec::new(),
        removed_call_edges: Vec::new(),
        warnings: vec!["parse_deferred_by_budget".to_string()],
    }
}

fn build_deleted_diff_file(
    root: &Path,
    mode: &ResolvedDiffDigestMode,
    summary_db: Option<&SummaryDb>,
    file_path: &Path,
) -> Result<DiffDigestFile> {
    let rel_path = relative_git_path(root, file_path);
    let previous_bytes = load_previous_bytes(root, mode, file_path)?;
    let previous = previous_bytes
        .as_deref()
        .map(|bytes| parse_snapshot(file_path, bytes))
        .unwrap_or_default();
    let touched_symbols = previous.symbol_names.clone();
    let touched_headings = previous.heading_names.clone();
    let content_hash = previous_bytes.as_deref().map(summarize::content_hash);
    let (summary_state, current_summaries) = collect_current_summaries(
        summary_db,
        &rel_path,
        content_hash.as_deref(),
        &touched_symbols,
    )?;

    Ok(DiffDigestFile {
        path: rel_path,
        status: DiffDigestFileStatus::Deleted,
        touched_symbols,
        touched_headings,
        summary_state,
        current_summaries,
        added_call_edges: Vec::new(),
        removed_call_edges: previous.edges.into_iter().collect(),
        warnings: previous.warnings,
    })
}

fn collect_current_summaries(
    summary_db: Option<&SummaryDb>,
    rel_path: &str,
    content_hash: Option<&str>,
    touched_symbols: &[String],
) -> Result<(DiffDigestSummaryState, Vec<DiffDigestSummarySnippet>)> {
    let Some(summary_db) = summary_db else {
        return Ok((DiffDigestSummaryState::Unavailable, Vec::new()));
    };

    let rows = summary_db.get_by_file(rel_path)?;
    if rows.is_empty() {
        return Ok((DiffDigestSummaryState::Missing, Vec::new()));
    }

    let Some(content_hash) = content_hash else {
        return Ok((DiffDigestSummaryState::Stale, Vec::new()));
    };

    if !summary_db.is_current(rel_path, content_hash)? {
        return Ok((DiffDigestSummaryState::Stale, Vec::new()));
    }

    let requested_symbols = touched_symbols.iter().cloned().collect::<BTreeSet<_>>();
    let mut snippets = Vec::new();
    let mut seen_symbols = BTreeSet::new();

    for row in &rows {
        if !requested_symbols.is_empty() && !requested_symbols.contains(&row.symbol_name) {
            continue;
        }
        if seen_symbols.insert(row.symbol_name.clone()) {
            snippets.push(DiffDigestSummarySnippet {
                symbol: row.symbol_name.clone(),
                summary: row.summary.trim().to_string(),
            });
        }
        if snippets.len() == 2 {
            break;
        }
    }

    if snippets.is_empty() {
        for row in &rows {
            if seen_symbols.insert(row.symbol_name.clone()) {
                snippets.push(DiffDigestSummarySnippet {
                    symbol: row.symbol_name.clone(),
                    summary: row.summary.trim().to_string(),
                });
            }
            if snippets.len() == 2 {
                break;
            }
        }
    }

    Ok((DiffDigestSummaryState::Current, snippets))
}

fn parse_snapshot(file_path: &Path, bytes: &[u8]) -> ParsedSnapshot {
    let Some(lang) = file_path
        .extension()
        .and_then(|ext| ext.to_str())
        .and_then(Lang::from_extension)
    else {
        return ParsedSnapshot::default();
    };

    let mut warnings = Vec::new();
    let symbols = match lang.extract_symbols(bytes) {
        Ok(symbols) => symbols,
        Err(err) => {
            warnings.push(format!("symbol extraction failed: {err}"));
            Vec::new()
        }
    };
    // #docsym: a document language's structural nodes are headings, list items,
    // and fenced blocks. Reporting them as `touched_symbols` made a docs-only
    // diff read as 40 changed symbols, and the list-item and prose entries
    // crowded real symbol churn out of a mixed diff's budget. Keep only the
    // headings, and report them in their own field.
    let (symbol_names, heading_names) = if lang.is_document() {
        let headings = symbols
            .iter()
            .filter(|symbol| symbol.kind == "heading")
            .map(|symbol| symbol.name.clone())
            .collect::<Vec<_>>();
        (Vec::new(), headings)
    } else {
        let names = symbols
            .iter()
            .map(|symbol| symbol.name.clone())
            .collect::<Vec<_>>();
        (names, Vec::new())
    };

    let edges = match graph::extract_call_sites(lang, bytes) {
        Ok(call_sites) => graph::resolve_edges(&symbols, &call_sites)
            .into_iter()
            .map(|edge| format!("{} -> {}", edge.caller, edge.callee))
            .collect(),
        Err(err) => {
            warnings.push(format!("call-edge extraction failed: {err}"));
            BTreeSet::new()
        }
    };

    ParsedSnapshot {
        symbol_names,
        heading_names,
        edges,
        warnings,
    }
}

fn merge_symbol_names(previous: &[String], current: &[String]) -> Vec<String> {
    previous
        .iter()
        .chain(current.iter())
        .cloned()
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect()
}

fn diff_edges(current: &BTreeSet<String>, previous: &BTreeSet<String>) -> Vec<String> {
    current.difference(previous).cloned().collect()
}

fn relative_git_path(root: &Path, file_path: &Path) -> String {
    summarize::normalize_summary_file_key(file_path.strip_prefix(root).unwrap_or(file_path))
}

fn is_internal_tsift_artifact(root: &Path, file_path: &Path) -> bool {
    let rel_path = relative_git_path(root, file_path);
    rel_path == ".tsift" || rel_path.starts_with(".tsift/")
}

fn git_file_bytes(root: &Path, git_ref: &str, file_path: &Path) -> Result<Option<Vec<u8>>> {
    let rel_path = relative_git_path(root, file_path);
    let output = Command::new("git")
        .args(["show", &format!("{git_ref}:{rel_path}")])
        .current_dir(root)
        .output()
        .with_context(|| format!("running git show {git_ref}:{rel_path}"))?;

    if output.status.success() {
        return Ok(Some(output.stdout));
    }

    Ok(None)
}

fn git_index_file_bytes(root: &Path, file_path: &Path) -> Result<Option<Vec<u8>>> {
    let rel_path = relative_git_path(root, file_path);
    let output = Command::new("git")
        .args(["show", &format!(":{rel_path}")])
        .current_dir(root)
        .output()
        .with_context(|| format!("running git show :{rel_path}"))?;

    if output.status.success() {
        return Ok(Some(output.stdout));
    }

    Ok(None)
}

#[derive(Debug, Clone)]
enum ResolvedDiffDigestMode {
    WorkingTree,
    Cached,
    Revision(RevisionBounds),
}

impl ResolvedDiffDigestMode {
    fn report_mode(&self) -> DiffDigestMode {
        match self {
            Self::WorkingTree => DiffDigestMode::WorkingTree,
            Self::Cached => DiffDigestMode::Cached,
            Self::Revision(_) => DiffDigestMode::Revision,
        }
    }

    fn report_revision(&self) -> Option<String> {
        match self {
            Self::Revision(bounds) => Some(bounds.target.clone()),
            _ => None,
        }
    }

    fn previous_source(&self) -> Option<SnapshotSource> {
        match self {
            Self::WorkingTree | Self::Cached => Some(SnapshotSource::GitRef("HEAD".to_string())),
            Self::Revision(bounds) => bounds
                .base
                .as_ref()
                .map(|base| SnapshotSource::GitRef(base.clone())),
        }
    }

    fn current_source(&self) -> SnapshotSource {
        match self {
            Self::WorkingTree => SnapshotSource::WorkingTree,
            Self::Cached => SnapshotSource::Index,
            Self::Revision(bounds) => SnapshotSource::GitRef(bounds.target.clone()),
        }
    }
}

fn resolve_mode(root: &Path, options: DiffDigestOptions<'_>) -> Result<ResolvedDiffDigestMode> {
    match (options.cached, options.revision) {
        (true, Some(_)) => {
            anyhow::bail!("diff-digest accepts either --cached or --revision, not both")
        }
        (true, None) => Ok(ResolvedDiffDigestMode::Cached),
        (false, Some(revision)) => Ok(ResolvedDiffDigestMode::Revision(resolve_revision_bounds(
            root, revision,
        )?)),
        (false, None) => Ok(ResolvedDiffDigestMode::WorkingTree),
    }
}

fn collect_changed_files(
    root: &Path,
    mode: &ResolvedDiffDigestMode,
) -> Result<summarize::GitChangedFiles> {
    match mode {
        ResolvedDiffDigestMode::WorkingTree => summarize::git_changed_files(root),
        ResolvedDiffDigestMode::Cached => git_changed_files_from_args(
            root,
            if git_has_head_commit(root)? {
                &[
                    "diff",
                    "--cached",
                    "--name-status",
                    "--find-renames",
                    "HEAD",
                ]
            } else {
                &[
                    "diff",
                    "--cached",
                    "--name-status",
                    "--find-renames",
                    "--root",
                ]
            },
            "git diff --cached --name-status",
        ),
        ResolvedDiffDigestMode::Revision(bounds) => git_changed_files_for_revision(root, bounds),
    }
}

fn git_changed_files_for_revision(
    root: &Path,
    bounds: &RevisionBounds,
) -> Result<summarize::GitChangedFiles> {
    if let Some(base) = &bounds.base {
        return git_changed_files_from_args(
            root,
            &[
                "diff",
                "--name-status",
                "--find-renames",
                base,
                &bounds.target,
            ],
            "git diff --name-status",
        );
    }

    git_changed_files_from_args(
        root,
        &[
            "diff-tree",
            "--root",
            "--no-commit-id",
            "-r",
            "--name-status",
            "--find-renames",
            &bounds.target,
        ],
        "git diff-tree --root --name-status",
    )
}

fn git_changed_files_from_args(
    root: &Path,
    args: &[&str],
    label: &str,
) -> Result<summarize::GitChangedFiles> {
    let output = Command::new("git")
        .args(args)
        .current_dir(root)
        .output()
        .with_context(|| format!("running {label}"))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!("{label} failed: {}", stderr.trim());
    }

    let mut existing = Vec::new();
    let mut deleted = Vec::new();
    for line in String::from_utf8_lossy(&output.stdout).lines() {
        if line.is_empty() {
            continue;
        }
        let mut fields = line.split('\t');
        let status = fields.next().unwrap_or_default();
        match status.chars().next() {
            Some('D') => {
                let path = fields
                    .next()
                    .with_context(|| format!("parsing deleted git path: {line}"))?;
                deleted.push(root.join(path));
            }
            Some('R') => {
                let old_path = fields
                    .next()
                    .with_context(|| format!("parsing renamed git old path: {line}"))?;
                let new_path = fields
                    .next()
                    .with_context(|| format!("parsing renamed git new path: {line}"))?;
                deleted.push(root.join(old_path));
                existing.push(root.join(new_path));
            }
            Some(_) => {
                let path = fields
                    .next_back()
                    .or_else(|| fields.next())
                    .with_context(|| format!("parsing changed git path: {line}"))?;
                existing.push(root.join(path));
            }
            None => {}
        }
    }

    existing.sort();
    existing.dedup();
    deleted.sort();
    deleted.dedup();
    Ok(summarize::GitChangedFiles { existing, deleted })
}

fn load_previous_bytes(
    root: &Path,
    mode: &ResolvedDiffDigestMode,
    file_path: &Path,
) -> Result<Option<Vec<u8>>> {
    let Some(source) = mode.previous_source() else {
        return Ok(None);
    };
    load_snapshot_bytes(root, file_path, &source)
}

fn load_current_bytes(
    root: &Path,
    mode: &ResolvedDiffDigestMode,
    file_path: &Path,
) -> (Option<Vec<u8>>, Vec<String>) {
    match load_snapshot_bytes(root, file_path, &mode.current_source()) {
        Ok(bytes) => (bytes, Vec::new()),
        Err(err) if matches!(mode.current_source(), SnapshotSource::WorkingTree) => {
            (None, vec![format!("reading current file failed: {err}")])
        }
        Err(err) => (
            None,
            vec![format!("loading current snapshot failed: {err}")],
        ),
    }
}

fn load_snapshot_bytes(
    root: &Path,
    file_path: &Path,
    source: &SnapshotSource,
) -> Result<Option<Vec<u8>>> {
    match source {
        SnapshotSource::WorkingTree => {
            Ok(Some(std::fs::read(file_path).with_context(|| {
                format!("reading working tree file {}", file_path.display())
            })?))
        }
        SnapshotSource::Index => git_index_file_bytes(root, file_path),
        SnapshotSource::GitRef(git_ref) => git_file_bytes(root, git_ref, file_path),
    }
}

fn resolve_revision_bounds(root: &Path, revision: &str) -> Result<RevisionBounds> {
    let output = Command::new("git")
        .args(["rev-list", "--parents", "-n", "1", revision])
        .current_dir(root)
        .output()
        .with_context(|| format!("running git rev-list for {revision}"))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!("git rev-list --parents failed: {}", stderr.trim());
    }

    let line = String::from_utf8_lossy(&output.stdout);
    let fields = line.split_whitespace().collect::<Vec<_>>();
    if fields.is_empty() {
        anyhow::bail!("git rev-list returned no commit for revision `{revision}`");
    }

    let target = fields[0].to_string();
    let base = fields.get(1).map(|parent| (*parent).to_string());
    Ok(RevisionBounds { base, target })
}

fn git_has_head_commit(root: &Path) -> Result<bool> {
    let verify_head = Command::new("git")
        .args(["rev-parse", "--verify", "--quiet", "HEAD"])
        .current_dir(root)
        .output()
        .with_context(|| "running git rev-parse --verify HEAD")?;

    Ok(verify_head.status.success())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tsift_summarize::summarize::Summary;

    fn init_git_repo(path: &Path) {
        let status = Command::new("git")
            .args(["init"])
            .current_dir(path)
            .status()
            .unwrap();
        assert!(status.success(), "git init failed");

        let status = Command::new("git")
            .args(["add", "."])
            .current_dir(path)
            .status()
            .unwrap();
        assert!(status.success(), "git add failed");

        let status = Command::new("git")
            .args([
                "-c",
                "user.name=tsift-tests",
                "-c",
                "user.email=tsift-tests@example.com",
                "commit",
                "--quiet",
                "-m",
                "init",
            ])
            .current_dir(path)
            .status()
            .unwrap();
        assert!(status.success(), "git commit failed");
    }

    // #docsym regression: a docs-only diff used to report every heading, list
    // item, and clipped prose line as a "touched symbol" — 40 of them for one
    // added runbook — which is more output than `git diff --stat` and less
    // signal.
    #[test]
    fn diff_digest_reports_markdown_headings_apart_from_symbols() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("main.rs"), "fn main() {}\n").unwrap();
        init_git_repo(dir.path());

        std::fs::write(
            dir.path().join("runbook.md"),
            "# Code Navigation\n\nSome prose that is long enough to have been clipped into a \
             fragment that looks like a symbol name.\n\n- `tsift --envelope source-read <file> \
             --budget normal` reads a window\n- `tsift --envelope search <query>` searches\n\n\
             ## Session start\n\nMore prose.\n",
        )
        .unwrap();

        let report = compute(dir.path(), DiffDigestOptions::default()).unwrap();
        let file = report
            .files
            .iter()
            .find(|file| file.path == "runbook.md")
            .expect("markdown file in the digest");

        assert!(
            file.touched_symbols.is_empty(),
            "markdown structure is not symbols: {:?}",
            file.touched_symbols
        );
        assert_eq!(
            file.touched_headings,
            vec!["Code Navigation".to_string(), "Session start".to_string()],
            "headings are the document's structural summary"
        );
        assert_eq!(report.symbols_touched, 0, "no code changed in this diff");
        assert_eq!(report.headings_touched, 2);
    }

    #[test]
    fn diff_digest_reports_symbol_and_call_edge_deltas() {
        let dir = tempfile::tempdir().unwrap();
        let file_path = dir.path().join("main.rs");
        std::fs::write(
            &file_path,
            "fn old_helper() {}\nfn main() { old_helper(); }\n",
        )
        .unwrap();
        init_git_repo(dir.path());

        let current = "fn new_helper() {}\nfn main() { new_helper(); }\n";
        std::fs::write(&file_path, current).unwrap();

        let summary_db = SummaryDb::open(&dir.path().join(".tsift/summaries.db")).unwrap();
        let current_hash = summarize::content_hash(current.as_bytes());
        for symbol_name in ["main", "new_helper"] {
            summary_db
                .insert(&Summary {
                    id: 0,
                    symbol_name: symbol_name.to_string(),
                    file_path: "main.rs".to_string(),
                    content_hash: current_hash.clone(),
                    summary: format!("{symbol_name} summary"),
                    entities: None,
                    relationships: None,
                    concept_labels: None,
                    extracted_at: "0".to_string(),
                    model: "test".to_string(),
                    tokens_input: Some(1),
                    tokens_output: Some(1),
                })
                .unwrap();
        }

        let report = compute(dir.path(), DiffDigestOptions::default()).unwrap();
        assert_eq!(report.files_changed, 1);
        assert_eq!(report.mode, DiffDigestMode::WorkingTree);
        assert_eq!(report.revision, None);
        assert_eq!(report.files_with_current_summaries, 1);
        assert_eq!(report.symbols_touched, 3);
        assert_eq!(report.call_edges_added, 1);
        assert_eq!(report.call_edges_removed, 1);

        let file = &report.files[0];
        assert_eq!(file.path, "main.rs");
        assert_eq!(file.status, DiffDigestFileStatus::Modified);
        assert_eq!(
            file.touched_symbols,
            vec![
                "main".to_string(),
                "new_helper".to_string(),
                "old_helper".to_string()
            ]
        );
        assert_eq!(
            file.current_summaries,
            vec![
                DiffDigestSummarySnippet {
                    symbol: "main".to_string(),
                    summary: "main summary".to_string()
                },
                DiffDigestSummarySnippet {
                    symbol: "new_helper".to_string(),
                    summary: "new_helper summary".to_string()
                }
            ]
        );
        assert_eq!(
            file.added_call_edges,
            vec!["main -> new_helper".to_string()]
        );
        assert_eq!(
            file.removed_call_edges,
            vec!["main -> old_helper".to_string()]
        );
    }

    #[test]
    fn diff_digest_cached_uses_index_snapshot_instead_of_working_tree() {
        let dir = tempfile::tempdir().unwrap();
        let file_path = dir.path().join("main.rs");
        std::fs::write(
            &file_path,
            "fn old_helper() {}\nfn main() { old_helper(); }\n",
        )
        .unwrap();
        init_git_repo(dir.path());

        std::fs::write(
            &file_path,
            "fn staged_helper() {}\nfn main() { staged_helper(); }\n",
        )
        .unwrap();
        let status = Command::new("git")
            .args(["add", "main.rs"])
            .current_dir(dir.path())
            .status()
            .unwrap();
        assert!(status.success(), "git add failed");

        std::fs::write(
            &file_path,
            "fn unstaged_helper() {}\nfn main() { unstaged_helper(); }\n",
        )
        .unwrap();

        let report = compute(
            dir.path(),
            DiffDigestOptions {
                cached: true,
                revision: None,
                max_parsed_files: None,
            },
        )
        .unwrap();
        assert_eq!(report.mode, DiffDigestMode::Cached);
        let file = &report.files[0];
        assert!(
            file.touched_symbols
                .iter()
                .any(|symbol| symbol == "staged_helper")
        );
        assert!(
            !file
                .touched_symbols
                .iter()
                .any(|symbol| symbol == "unstaged_helper")
        );
        assert_eq!(
            file.added_call_edges,
            vec!["main -> staged_helper".to_string()]
        );
    }

    #[test]
    fn diff_digest_revision_uses_commit_snapshots() {
        let dir = tempfile::tempdir().unwrap();
        let file_path = dir.path().join("main.rs");
        std::fs::write(
            &file_path,
            "fn old_helper() {}\nfn main() { old_helper(); }\n",
        )
        .unwrap();
        init_git_repo(dir.path());

        std::fs::write(
            &file_path,
            "fn committed_helper() {}\nfn main() { committed_helper(); }\n",
        )
        .unwrap();
        let status = Command::new("git")
            .args(["add", "main.rs"])
            .current_dir(dir.path())
            .status()
            .unwrap();
        assert!(status.success(), "git add failed");
        let status = Command::new("git")
            .args([
                "-c",
                "user.name=tsift-tests",
                "-c",
                "user.email=tsift-tests@example.com",
                "commit",
                "--quiet",
                "-m",
                "second",
            ])
            .current_dir(dir.path())
            .status()
            .unwrap();
        assert!(status.success(), "git commit failed");

        std::fs::write(
            &file_path,
            "fn working_tree_only() {}\nfn main() { working_tree_only(); }\n",
        )
        .unwrap();

        let report = compute(
            dir.path(),
            DiffDigestOptions {
                cached: false,
                revision: Some("HEAD"),
                max_parsed_files: None,
            },
        )
        .unwrap();
        assert_eq!(report.mode, DiffDigestMode::Revision);
        assert!(report.revision.as_deref().is_some());
        let file = &report.files[0];
        assert!(
            file.touched_symbols
                .iter()
                .any(|symbol| symbol == "committed_helper")
        );
        assert!(
            !file
                .touched_symbols
                .iter()
                .any(|symbol| symbol == "working_tree_only")
        );
        assert_eq!(
            file.added_call_edges,
            vec!["main -> committed_helper".to_string()]
        );
    }

    /// #gdbprephot: cap working-tree parsing to the caller's budget. Files
    /// beyond `max_parsed_files` get cheap path-only entries so context-pack
    /// preview cost scales with the preview window, not the working-tree
    /// change count.
    #[test]
    fn diff_digest_max_parsed_files_skips_tree_sitter_beyond_budget() {
        let dir = tempfile::tempdir().unwrap();
        // Create 5 modifiable files committed at baseline, then mutate each so
        // they all appear in the working-tree diff.
        for name in ["a.rs", "b.rs", "c.rs", "d.rs", "e.rs"] {
            std::fs::write(
                dir.path().join(name),
                format!(
                    "fn {}_helper() {{}}\nfn main() {{ {0}_helper(); }}\n",
                    name.trim_end_matches(".rs")
                ),
            )
            .unwrap();
        }
        init_git_repo(dir.path());
        for name in ["a.rs", "b.rs", "c.rs", "d.rs", "e.rs"] {
            std::fs::write(
                dir.path().join(name),
                format!(
                    "fn {0}_helper_v2() {{}}\nfn main() {{ {0}_helper_v2(); }}\n",
                    name.trim_end_matches(".rs")
                ),
            )
            .unwrap();
        }

        // No budget: every file gets a full parse.
        let full = compute(dir.path(), DiffDigestOptions::default()).unwrap();
        assert_eq!(full.files_changed, 5);
        assert!(
            full.symbols_touched >= 5,
            "full parse should touch every helper symbol: {full:?}"
        );
        let total_added: usize = full.files.iter().map(|f| f.added_call_edges.len()).sum();
        let total_removed: usize = full.files.iter().map(|f| f.removed_call_edges.len()).sum();
        assert!(
            total_added > 0 && total_removed > 0,
            "full parse should yield call-edge diffs: {full:?}"
        );
        assert!(
            full.files
                .iter()
                .all(|f| !f.warnings.contains(&"parse_deferred_by_budget".to_string()))
        );

        // Budget=2: first two files parsed, remaining three deferred.
        let bounded = compute(
            dir.path(),
            DiffDigestOptions {
                cached: false,
                revision: None,
                max_parsed_files: Some(2),
            },
        )
        .unwrap();
        assert_eq!(
            bounded.files_changed, 5,
            "files_changed must count every path"
        );
        assert!(
            bounded.symbols_touched <= full.symbols_touched,
            "bounded symbol count must not exceed full parse"
        );
        let parsed: Vec<&DiffDigestFile> = bounded
            .files
            .iter()
            .filter(|f| !f.warnings.contains(&"parse_deferred_by_budget".to_string()))
            .collect();
        let deferred: Vec<&DiffDigestFile> = bounded
            .files
            .iter()
            .filter(|f| f.warnings.contains(&"parse_deferred_by_budget".to_string()))
            .collect();
        assert_eq!(
            parsed.len(),
            2,
            "exactly two files should be parsed: {bounded:?}"
        );
        assert_eq!(
            deferred.len(),
            3,
            "remaining three files should be deferred: {bounded:?}"
        );
        for f in &deferred {
            assert!(
                f.touched_symbols.is_empty(),
                "deferred file leaked symbols: {f:?}"
            );
            assert!(
                f.added_call_edges.is_empty(),
                "deferred file leaked added edges: {f:?}"
            );
            assert!(
                f.removed_call_edges.is_empty(),
                "deferred file leaked removed edges: {f:?}"
            );
            assert_eq!(f.summary_state, DiffDigestSummaryState::Unavailable);
        }
        // Parsing must follow canonical sort: parsed files come first in `files`
        // (sorted by path) and match the first two of the full parse.
        assert_eq!(
            bounded
                .files
                .iter()
                .take(2)
                .map(|f| f.path.clone())
                .collect::<Vec<_>>(),
            full.files
                .iter()
                .take(2)
                .map(|f| f.path.clone())
                .collect::<Vec<_>>(),
        );
    }
}
