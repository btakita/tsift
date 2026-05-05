use crate::graph;
use crate::lang::Lang;
use crate::lint;
use crate::summarize::{self, SummaryDb};
use anyhow::{Context, Result};
use serde::Serialize;
use std::collections::BTreeSet;
use std::path::Path;
use std::process::Command;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DiffDigestFileStatus {
    Added,
    Modified,
    Deleted,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DiffDigestSummaryState {
    Current,
    Stale,
    Missing,
    Unavailable,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct DiffDigestSummarySnippet {
    pub symbol: String,
    pub summary: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct DiffDigestFile {
    pub path: String,
    pub status: DiffDigestFileStatus,
    pub touched_symbols: Vec<String>,
    pub summary_state: DiffDigestSummaryState,
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub current_summaries: Vec<DiffDigestSummarySnippet>,
    pub added_call_edges: Vec<String>,
    pub removed_call_edges: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub warnings: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct DiffDigestReport {
    pub root: String,
    pub files_changed: usize,
    pub files_with_current_summaries: usize,
    pub symbols_touched: usize,
    pub call_edges_added: usize,
    pub call_edges_removed: usize,
    pub files: Vec<DiffDigestFile>,
}

#[derive(Debug, Default)]
struct ParsedSnapshot {
    symbol_names: Vec<String>,
    edges: BTreeSet<String>,
    warnings: Vec<String>,
}

pub fn compute(path: &Path) -> Result<DiffDigestReport> {
    let root = lint::resolve_project_root_or_canonical_path(path)?;
    let changed = summarize::git_changed_files(&root)?;
    let summary_db = open_summary_db_if_present(&root)?;

    let mut files = Vec::new();

    for file_path in changed.existing {
        if is_internal_tsift_artifact(&root, &file_path) {
            continue;
        }
        let previous = git_head_file_bytes(&root, &file_path)?;
        let status = if previous.is_some() {
            DiffDigestFileStatus::Modified
        } else {
            DiffDigestFileStatus::Added
        };
        files.push(build_diff_file(
            &root,
            summary_db.as_ref(),
            &file_path,
            status,
            previous.as_deref(),
        )?);
    }

    for file_path in changed.deleted {
        if is_internal_tsift_artifact(&root, &file_path) {
            continue;
        }
        files.push(build_deleted_diff_file(
            &root,
            summary_db.as_ref(),
            &file_path,
        )?);
    }

    files.sort_by(|left, right| left.path.cmp(&right.path));

    let files_with_current_summaries = files
        .iter()
        .filter(|file| file.summary_state == DiffDigestSummaryState::Current)
        .count();
    let symbols_touched = files.iter().map(|file| file.touched_symbols.len()).sum();
    let call_edges_added = files.iter().map(|file| file.added_call_edges.len()).sum();
    let call_edges_removed = files.iter().map(|file| file.removed_call_edges.len()).sum();

    Ok(DiffDigestReport {
        root: root.display().to_string(),
        files_changed: files.len(),
        files_with_current_summaries,
        symbols_touched,
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
) -> Result<DiffDigestFile> {
    let rel_path = relative_git_path(root, file_path);
    let mut warnings = Vec::new();
    let current_bytes = match std::fs::read(file_path) {
        Ok(bytes) => Some(bytes),
        Err(err) => {
            warnings.push(format!("reading current file failed: {err}"));
            None
        }
    };

    let previous = previous_bytes
        .map(|bytes| parse_snapshot(file_path, bytes))
        .unwrap_or_default();
    let current = current_bytes
        .as_deref()
        .map(|bytes| parse_snapshot(file_path, bytes))
        .unwrap_or_default();
    warnings.extend(previous.warnings);
    warnings.extend(current.warnings);

    let touched_symbols = merge_symbol_names(&previous.symbol_names, &current.symbol_names);
    let added_call_edges = diff_edges(&current.edges, &previous.edges);
    let removed_call_edges = diff_edges(&previous.edges, &current.edges);
    let content_hash = current_bytes
        .as_deref()
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
        summary_state,
        current_summaries,
        added_call_edges,
        removed_call_edges,
        warnings,
    })
}

fn build_deleted_diff_file(
    root: &Path,
    summary_db: Option<&SummaryDb>,
    file_path: &Path,
) -> Result<DiffDigestFile> {
    let rel_path = relative_git_path(root, file_path);
    let previous_bytes = git_head_file_bytes(root, file_path)?;
    let previous = previous_bytes
        .as_deref()
        .map(|bytes| parse_snapshot(file_path, bytes))
        .unwrap_or_default();
    let touched_symbols = previous.symbol_names.clone();
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
    let symbol_names = symbols
        .iter()
        .map(|symbol| symbol.name.clone())
        .collect::<Vec<_>>();

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

fn git_head_file_bytes(root: &Path, file_path: &Path) -> Result<Option<Vec<u8>>> {
    let rel_path = relative_git_path(root, file_path);
    let output = Command::new("git")
        .args(["show", &format!("HEAD:{rel_path}")])
        .current_dir(root)
        .output()
        .with_context(|| format!("running git show for {rel_path}"))?;

    if output.status.success() {
        return Ok(Some(output.stdout));
    }

    Ok(None)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::summarize::Summary;

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

        let report = compute(dir.path()).unwrap();
        assert_eq!(report.files_changed, 1);
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
}
