use serde::Serialize;
use std::cmp::Ordering;
use std::collections::{BTreeSet, HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};

use tsift_index::index;
use tsift_search::sift;
use tsift_summarize::summarize;

use crate::output::ResponseBudget;
use crate::{
    compact_snippet, dedupe_preserve_order, format_score, format_symbol_preview_line,
    canonical_tag_family_from_symbol, family_query_from_tag_alias, relativize,
    resolve_query_db_path, shell_quote, source_read_command, source_symbol_read_command,
    source_symbol_line, stable_handle, stored_symbol_ast_span, stored_symbol_span_bounds,
    stored_symbol_span_handle, symbol_hit_ast_span, symbol_hit_line, symbol_hit_span_bounds,
    markdown_ast_command, truncate_for_budget,
    AstSpanPreview, SearchFacetFilters,
};

#[derive(Serialize)]
pub(crate) struct SearchBudgetSymbolPreview {
    pub(crate) handle: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) tag_alias: Option<String>,
    pub(crate) match_type: String,
    pub(crate) kind: String,
    pub(crate) language: String,
    pub(crate) name: String,
    pub(crate) file: String,
    pub(crate) line: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) end_line: Option<i64>,
    pub(crate) score: f64,
    pub(crate) match_count: usize,
    pub(crate) surface_count: usize,
    pub(crate) file_count: usize,
    #[serde(skip_serializing_if = "is_zero_usize")]
    pub(crate) summary_refs: usize,
    #[serde(skip_serializing_if = "is_zero_usize")]
    pub(crate) graph_neighbors: usize,
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub(crate) surface_examples: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) ast: Option<SearchBudgetAstArtifact>,
    pub(crate) expand: String,
}

#[derive(Serialize)]
pub(crate) struct SearchBudgetAstArtifact {
    pub(crate) artifact_kind: String,
    pub(crate) span: AstSpanPreview,
    pub(crate) expand: SearchBudgetAstExpandCommands,
}

#[derive(Serialize)]
pub(crate) struct SearchBudgetAstExpandCommands {
    pub(crate) source_window: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) source_body: Option<String>,
    pub(crate) symbol_read: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) markdown_ast: Option<String>,
}

#[derive(Serialize)]
pub(crate) struct SearchBudgetHitPreview {
    pub(crate) handle: String,
    pub(crate) rank: usize,
    pub(crate) path: String,
    pub(crate) confidence: String,
    pub(crate) score: f64,
    pub(crate) preview: String,
    pub(crate) expand: String,
}

#[derive(Serialize)]
pub(crate) struct SearchBudgetRankingProfile {
    pub(crate) mode: String,
    pub(crate) symbol_span_weight: f64,
    pub(crate) lexical_file_weight: f64,
    pub(crate) summary_boost: f64,
    pub(crate) graph_boost: f64,
}

#[derive(Serialize)]
pub(crate) struct SearchBudgetRankedPreview {
    pub(crate) handle: String,
    pub(crate) rank: usize,
    pub(crate) source: String,
    pub(crate) score: f64,
    pub(crate) path: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) line: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) kind: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) language: Option<String>,
    pub(crate) preview: String,
    pub(crate) reasons: Vec<String>,
    pub(crate) expand: String,
}

#[derive(Serialize)]
pub(crate) struct SearchScaleSignals {
    pub(crate) preview_symbols: usize,
    pub(crate) symbol_families: usize,
    pub(crate) raw_symbol_matches: usize,
    pub(crate) preview_hits: usize,
    pub(crate) returned_hits: usize,
    pub(crate) indexed_artifacts: usize,
    pub(crate) skipped_artifacts: usize,
    pub(crate) max_items: usize,
    pub(crate) max_bytes: usize,
}

#[derive(Serialize)]
pub(crate) struct SearchScaleGuard {
    pub(crate) level: String,
    pub(crate) warning: String,
    pub(crate) signals: SearchScaleSignals,
    pub(crate) narrow_commands: Vec<String>,
}

#[derive(Serialize)]
pub(crate) struct SearchBudgetReport {
    pub(crate) query: String,
    pub(crate) strategy: String,
    #[serde(skip_serializing_if = "SearchFacetFilters::is_empty", default)]
    pub(crate) filters: SearchFacetFilters,
    pub(crate) indexed_artifacts: usize,
    pub(crate) skipped_artifacts: usize,
    pub(crate) max_items: usize,
    pub(crate) max_bytes: usize,
    pub(crate) symbol_total: usize,
    pub(crate) raw_symbol_total: usize,
    pub(crate) hit_total: usize,
    pub(crate) truncated: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) scale_guard: Option<SearchScaleGuard>,
    pub(crate) symbols: Vec<SearchBudgetSymbolPreview>,
    pub(crate) hits: Vec<SearchBudgetHitPreview>,
    pub(crate) ranking: SearchBudgetRankingProfile,
    pub(crate) ranked: Vec<SearchBudgetRankedPreview>,
}

pub(crate) struct SearchBudgetReportInput<'a> {
    pub(crate) query: &'a str,
    pub(crate) strategy: &'a str,
    pub(crate) root: &'a Path,
    pub(crate) response: &'a sift::SearchResponse,
    pub(crate) symbol_hits: &'a [index::SymbolHit],
    pub(crate) absolute: bool,
    pub(crate) budget: ResponseBudget,
    pub(crate) filters: &'a SearchFacetFilters,
}

const SEARCH_BUDGET_SURFACE_PREVIEW_LIMIT: usize = 3;

fn is_zero_usize(value: &usize) -> bool {
    *value == 0
}

struct SearchBudgetSymbolFamily {
    canonical_family: Option<String>,
    canonical_tag_alias: Option<String>,
    representative_name: String,
    representative_kind: String,
    representative_match_type: String,
    representative_hit: index::SymbolHit,
    representative_file: String,
    representative_line: i64,
    representative_score: f64,
    seen_surfaces: HashSet<String>,
    seen_files: HashSet<String>,
    surface_examples: Vec<String>,
    match_count: usize,
}

fn search_budget_family_query(tag_alias: Option<&str>, fallback_name: &str) -> String {
    if let Some(alias) = tag_alias
        && let Some(query) = family_query_from_tag_alias(alias)
    {
        return query;
    }
    fallback_name.to_string()
}

fn build_search_budget_family_expand(
    strategy: &str,
    path: &str,
    tag_alias: Option<&str>,
    fallback_name: &str,
) -> String {
    let query = search_budget_family_query(tag_alias, fallback_name);
    let effective_strategy = if strategy == "exact" {
        "lexical"
    } else {
        strategy
    };
    build_search_budget_follow_up(&query, effective_strategy, path)
}

fn format_search_budget_symbol_name(name: &str, surface_count: usize, max_bytes: usize) -> String {
    let preview = if surface_count > 1 {
        let extra = surface_count - 1;
        let label = if extra == 1 { "variant" } else { "variants" };
        format!("{name} (+{extra} {label})")
    } else {
        name.to_string()
    };
    truncate_for_budget(&preview, max_bytes)
}

fn format_search_budget_symbol_file(file: &str, file_count: usize, max_bytes: usize) -> String {
    let preview = if file_count > 1 {
        let extra = file_count - 1;
        let label = if extra == 1 { "file" } else { "files" };
        format!("{file} (+{extra} {label})")
    } else {
        file.to_string()
    };
    truncate_for_budget(&preview, max_bytes)
}

pub(crate) fn build_search_budget_follow_up(query: &str, strategy: &str, path: &str) -> String {
    let mut command = format!(
        "tsift search {} --path {} --limit 20",
        shell_quote(query),
        shell_quote(path)
    );
    if strategy == "exact" {
        command.push_str(" --exact");
    } else if strategy != "lexical" {
        command.push_str(&format!(" --strategy {}", shell_quote(strategy)));
    }
    command
}

fn build_search_exact_narrow_command(query: &str, path: &str, max_items: usize) -> String {
    format!(
        "tsift search {} --path {} --limit {} --exact",
        shell_quote(query),
        shell_quote(path),
        max_items.max(1)
    )
}

fn build_search_path_narrow_command(query: &str, strategy: &str, path: &str) -> String {
    let mut command = format!(
        "tsift search {} --path {} --limit 20",
        shell_quote(query),
        shell_quote(path)
    );
    if strategy == "exact" {
        command.push_str(" --exact");
    } else if strategy != "lexical" {
        command.push_str(&format!(" --strategy {}", shell_quote(strategy)));
    }
    command
}

fn search_budget_symbol_source_path(root: &Path, file: &str) -> PathBuf {
    let path = Path::new(file);
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        root.join(path)
    }
}

fn search_budget_ast_expand_commands(
    root: &Path,
    symbol: &index::SymbolHit,
    span: &AstSpanPreview,
) -> SearchBudgetAstExpandCommands {
    let source_line_count = span
        .end_line
        .saturating_sub(span.start_line)
        .saturating_add(1)
        .max(1);
    let source_body = span
        .body_start_line
        .zip(span.body_end_line)
        .map(|(start, end)| {
            let line_count = end.saturating_sub(start).saturating_add(1).max(1);
            source_read_command(root, &symbol.file, start, line_count)
        });
    SearchBudgetAstExpandCommands {
        source_window: source_read_command(root, &symbol.file, span.start_line, source_line_count),
        source_body,
        symbol_read: source_symbol_read_command(root, &symbol.name, &symbol.file),
        markdown_ast: (symbol.language == "markdown")
            .then(|| markdown_ast_command(root, &symbol.file, Some(&span.handle))),
    }
}

fn search_budget_ast_artifact(
    root: &Path,
    symbol: &index::SymbolHit,
) -> Option<SearchBudgetAstArtifact> {
    let source = fs::read(search_budget_symbol_source_path(root, &symbol.file)).ok()?;
    let span = symbol_hit_ast_span(symbol, &source)?;
    let expand = search_budget_ast_expand_commands(root, symbol, &span);
    Some(SearchBudgetAstArtifact {
        artifact_kind: "ast_span".to_string(),
        span,
        expand,
    })
}

fn search_budget_summary_db(root: &Path) -> Option<summarize::SummaryDb> {
    let db_path = root.join(".tsift/summaries.db");
    if !db_path.exists() {
        return None;
    }
    summarize::SummaryDb::open_read_only_resilient(&db_path).ok()
}

fn search_budget_summary_path_candidates(root: &Path, file: &str) -> Vec<String> {
    let mut candidates = Vec::new();
    candidates.push(file.to_string());
    let relative = relativize(file, root);
    candidates.push(relative);
    let path = Path::new(file);
    if let Ok(stripped) = path.strip_prefix(root) {
        candidates.push(stripped.to_string_lossy().to_string());
    }
    candidates.sort();
    candidates.dedup();
    candidates
}

fn search_budget_summary_ref_count(
    summary_db: Option<&summarize::SummaryDb>,
    root: &Path,
    symbol: &index::SymbolHit,
) -> usize {
    let Some(summary_db) = summary_db else {
        return 0;
    };
    let mut ids = BTreeSet::new();
    if let Ok(summaries) = summary_db.get_by_symbol(&symbol.name) {
        for summary in summaries {
            ids.insert(summary.id);
        }
    }
    for path in search_budget_summary_path_candidates(root, &symbol.file) {
        if let Ok(summaries) = summary_db.get_by_file(&path) {
            for summary in summaries {
                if summary.symbol_name == symbol.name
                    || summary.symbol_name == path
                    || summary.symbol_name
                        == Path::new(&path)
                            .file_name()
                            .and_then(|name| name.to_str())
                            .unwrap_or_default()
                {
                    ids.insert(summary.id);
                }
            }
        }
    }
    ids.len()
}

fn search_budget_graph_neighbor_count(ast: Option<&SearchBudgetAstArtifact>) -> usize {
    let Some(ast) = ast else {
        return 0;
    };
    let mut count = ast.span.child_handles.len();
    if ast.span.parent_handle.is_some() {
        count += 1;
    }
    if let Some(markdown) = &ast.span.markdown {
        count += markdown.embedded_symbols.len();
    }
    count
}

fn search_budget_ranking_profile() -> SearchBudgetRankingProfile {
    SearchBudgetRankingProfile {
        mode: "ast_aware_merged".to_string(),
        symbol_span_weight: 1.0,
        lexical_file_weight: 0.45,
        summary_boost: 10.0,
        graph_boost: 12.0,
    }
}

fn search_budget_clamped_score(value: f64, min: f64, max: f64) -> f64 {
    if value.is_finite() {
        value.clamp(min, max)
    } else {
        min
    }
}

fn search_budget_symbol_rank_score(symbol: &SearchBudgetSymbolPreview) -> (f64, Vec<String>) {
    let mut score = 40.0 + (search_budget_clamped_score(symbol.score, 0.0, 1.0) * 50.0);
    let mut reasons = vec![format!("symbol:{}:{:.2}", symbol.match_type, symbol.score)];
    if symbol.match_type == "exact_name" {
        score += 10.0;
        reasons.push("exact_symbol_name".to_string());
    }
    if symbol.ast.is_some() {
        score += 20.0;
        reasons.push("ast_span".to_string());
    }
    if symbol.summary_refs > 0 {
        let boost = (symbol.summary_refs as f64 * 5.0).min(10.0);
        score += boost;
        reasons.push(format!("summary_refs:{}", symbol.summary_refs));
    }
    if symbol.graph_neighbors > 0 {
        let boost = (symbol.graph_neighbors as f64 * 4.0).min(12.0);
        score += boost;
        reasons.push(format!("graph_neighbors:{}", symbol.graph_neighbors));
    }
    (score, reasons)
}

fn search_budget_lexical_rank_score(hit: &SearchBudgetHitPreview) -> (f64, Vec<String>) {
    let capped = search_budget_clamped_score(hit.score, 0.0, 100.0);
    let mut score = (capped * 0.45).min(45.0);
    let mut reasons = vec![format!("lexical_file:{:.2}", hit.score)];
    if hit.confidence.eq_ignore_ascii_case("High") {
        score += 3.0;
        reasons.push("high_confidence_lexical".to_string());
    }
    (score, reasons)
}

fn build_search_budget_ranked_previews(
    query: &str,
    max_items: usize,
    max_bytes: usize,
    symbols: &[SearchBudgetSymbolPreview],
    hits: &[SearchBudgetHitPreview],
) -> Vec<SearchBudgetRankedPreview> {
    let mut ranked = Vec::new();
    for symbol in symbols {
        let (score, reasons) = search_budget_symbol_rank_score(symbol);
        let key = format!(
            "symbol:{}:{}:{}:{}:{}",
            symbol.handle, symbol.file, symbol.line, score, query
        );
        ranked.push(SearchBudgetRankedPreview {
            handle: stable_handle("srnk", &key),
            rank: 0,
            source: if symbol.ast.is_some() {
                "symbol_span".to_string()
            } else {
                "symbol".to_string()
            },
            score,
            path: symbol.file.clone(),
            line: Some(symbol.line),
            name: Some(symbol.name.clone()),
            kind: Some(symbol.kind.clone()),
            language: Some(symbol.language.clone()),
            preview: truncate_for_budget(
                &format!("{} {}", symbol.match_type, symbol.kind),
                max_bytes,
            ),
            reasons,
            expand: symbol.expand.clone(),
        });
    }
    for hit in hits {
        let (score, reasons) = search_budget_lexical_rank_score(hit);
        let key = format!("lexical:{}:{}:{}:{}", hit.handle, hit.path, score, query);
        ranked.push(SearchBudgetRankedPreview {
            handle: stable_handle("srnk", &key),
            rank: 0,
            source: "lexical_file".to_string(),
            score,
            path: hit.path.clone(),
            line: None,
            name: None,
            kind: None,
            language: None,
            preview: truncate_for_budget(&hit.preview, max_bytes),
            reasons,
            expand: hit.expand.clone(),
        });
    }
    ranked.sort_by(|left, right| {
        right
            .score
            .partial_cmp(&left.score)
            .unwrap_or(Ordering::Equal)
            .then_with(|| left.path.cmp(&right.path))
            .then_with(|| left.line.cmp(&right.line))
            .then_with(|| left.name.cmp(&right.name))
    });
    ranked.truncate(max_items);
    for (idx, item) in ranked.iter_mut().enumerate() {
        item.rank = idx + 1;
    }
    ranked
}

struct SearchFacetAstContext {
    ast: AstSpanPreview,
    parent_values: Vec<String>,
    child_values: Vec<String>,
}

fn normalized_facet_value(value: &str) -> String {
    value.trim().to_ascii_lowercase()
}

fn text_filter_matches_value(filter: &str, value: &str) -> bool {
    normalized_facet_value(filter) == normalized_facet_value(value)
}

fn text_filters_match_value(filters: &[String], value: Option<&str>) -> bool {
    if filters.is_empty() {
        return true;
    }
    value.is_some_and(|value| {
        filters
            .iter()
            .any(|filter| text_filter_matches_value(filter, value))
    })
}

fn text_filters_match_any_value(filters: &[String], values: &[String]) -> bool {
    filters.is_empty()
        || filters.iter().any(|filter| {
            values
                .iter()
                .any(|value| text_filter_matches_value(filter, value))
        })
}

fn symbol_hit_matches_scalar_facets(
    symbol: &index::SymbolHit,
    filters: &SearchFacetFilters,
) -> bool {
    text_filters_match_value(&filters.languages, Some(&symbol.language))
        && text_filters_match_value(&filters.kinds, Some(&symbol.kind))
        && text_filters_match_value(&filters.node_kinds, symbol.node_kind.as_deref())
}

fn search_facet_matching_stored_symbol<'a>(
    symbols: &'a [index::StoredSymbol],
    hit: &index::SymbolHit,
) -> Option<&'a index::StoredSymbol> {
    let hit_span = symbol_hit_span_bounds(hit);
    let hit_line = symbol_hit_line(hit);
    symbols.iter().find(|symbol| {
        if symbol.name != hit.name || symbol.kind != hit.kind {
            return false;
        }
        if let Some(hit_span) = hit_span {
            return stored_symbol_span_bounds(symbol) == Some(hit_span);
        }
        source_symbol_line(symbol) == hit_line
    })
}

fn search_facet_symbol_values(symbol: &index::StoredSymbol) -> Vec<String> {
    let mut values = vec![
        symbol.name.clone(),
        symbol.kind.clone(),
        symbol.language.clone(),
    ];
    if let Some(node_kind) = &symbol.node_kind {
        values.push(node_kind.clone());
    }
    if let Some(handle) = stored_symbol_span_handle(symbol) {
        values.push(handle);
    }
    values
}

fn search_facet_ast_context(
    root: &Path,
    symbol: &index::SymbolHit,
) -> Option<SearchFacetAstContext> {
    let file_path = search_budget_symbol_source_path(root, &symbol.file);
    let source = fs::read(&file_path).ok()?;
    let db_path = resolve_query_db_path(root, &file_path, None).ok()?;
    let db = index::IndexDb::open_read_only_resilient(&db_path).ok()?;
    let symbols = db.symbols_for_file(&file_path.to_string_lossy()).ok()?;
    let selected = search_facet_matching_stored_symbol(&symbols, symbol)?;
    let ast = stored_symbol_ast_span(selected, &source, &symbols, 64)?;

    let parent_values = ast
        .parent_handle
        .as_ref()
        .and_then(|parent_handle| {
            symbols.iter().find(|candidate| {
                stored_symbol_span_handle(candidate).as_ref() == Some(parent_handle)
            })
        })
        .map(search_facet_symbol_values)
        .unwrap_or_else(|| ast.parent_handle.clone().into_iter().collect());

    let mut child_values = Vec::new();
    for child_handle in &ast.child_handles {
        child_values.push(child_handle.clone());
        if let Some(child) = symbols
            .iter()
            .find(|candidate| stored_symbol_span_handle(candidate).as_ref() == Some(child_handle))
        {
            child_values.extend(search_facet_symbol_values(child));
        }
    }
    if let Some(markdown) = &ast.markdown {
        for embedded in &markdown.embedded_symbols {
            child_values.extend([
                embedded.handle.clone(),
                embedded.name.clone(),
                embedded.kind.clone(),
                embedded.language.clone(),
                embedded.node_kind.clone(),
            ]);
        }
    }
    child_values.sort();
    child_values.dedup();

    Some(SearchFacetAstContext {
        ast,
        parent_values,
        child_values,
    })
}

fn search_facet_context_matches(
    filters: &SearchFacetFilters,
    context: &SearchFacetAstContext,
) -> bool {
    let markdown = context.ast.markdown.as_ref();
    let mut section_values = Vec::new();
    if let Some(markdown) = markdown {
        section_values.extend(markdown.section_path.clone());
        if !markdown.section_path.is_empty() {
            section_values.push(markdown.section_path.join("/"));
            section_values.push(markdown.section_path.join(" > "));
        }
        if let Some(section_handle) = &markdown.section_handle {
            section_values.push(section_handle.clone());
        }
    }

    text_filters_match_any_value(&filters.sections, &section_values)
        && text_filters_match_any_value(&filters.parents, &context.parent_values)
        && text_filters_match_any_value(&filters.children, &context.child_values)
        && text_filters_match_value(
            &filters.fence_languages,
            markdown.and_then(|metadata| metadata.fence_language.as_deref()),
        )
        && (filters.list_depths.is_empty()
            || markdown
                .and_then(|metadata| metadata.list_depth)
                .is_some_and(|depth| filters.list_depths.contains(&depth)))
        && (filters.heading_levels.is_empty()
            || markdown
                .and_then(|metadata| metadata.heading_level)
                .is_some_and(|level| filters.heading_levels.contains(&level)))
}

fn symbol_hit_matches_search_facets(
    root: &Path,
    symbol: &index::SymbolHit,
    filters: &SearchFacetFilters,
) -> bool {
    if !symbol_hit_matches_scalar_facets(symbol, filters) {
        return false;
    }
    if !filters.needs_ast_context() {
        return true;
    }
    search_facet_ast_context(root, symbol)
        .as_ref()
        .is_some_and(|context| search_facet_context_matches(filters, context))
}

pub(crate) fn apply_search_facet_filters(
    root: &Path,
    hits: Vec<index::SymbolHit>,
    filters: &SearchFacetFilters,
) -> Vec<index::SymbolHit> {
    if filters.is_empty() {
        return hits;
    }
    hits.into_iter()
        .filter(|hit| symbol_hit_matches_search_facets(root, hit, filters))
        .collect()
}

#[allow(clippy::too_many_arguments)]
fn build_search_scale_guard(
    query: &str,
    strategy: &str,
    root: &Path,
    response: &sift::SearchResponse,
    symbol_total: usize,
    raw_symbol_total: usize,
    hit_total: usize,
    max_items: usize,
    max_bytes: usize,
    symbols: &[SearchBudgetSymbolPreview],
    hits: &[SearchBudgetHitPreview],
) -> Option<SearchScaleGuard> {
    let broad_symbols = symbol_total > max_items || raw_symbol_total > max_items;
    let broad_hits = hit_total > max_items;
    let broad_corpus = response
        .indexed_artifacts
        .saturating_add(response.skipped_artifacts)
        >= 250;
    if !broad_symbols && !broad_hits && !broad_corpus {
        return None;
    }

    let mut narrow_commands = Vec::new();
    let root_path = root.to_string_lossy();
    if strategy != "exact" {
        narrow_commands.push(build_search_exact_narrow_command(
            query,
            root_path.as_ref(),
            max_items,
        ));
    }
    if let Some(symbol) = symbols.first() {
        narrow_commands.push(symbol.expand.clone());
    }
    if let Some(hit) = hits.first() {
        narrow_commands.push(build_search_path_narrow_command(query, strategy, &hit.path));
    }
    narrow_commands.push(
        "tsift workflow search --json # preserve handles, expand only cited parents".to_string(),
    );

    Some(SearchScaleGuard {
        level: if broad_hits || broad_symbols {
            "high-hit".to_string()
        } else {
            "corpus-size".to_string()
        },
        warning: "Broad search surface: inspect the preview first and run a narrowing command before dispatching parallel agents."
            .to_string(),
        signals: SearchScaleSignals {
            preview_symbols: symbols.len(),
            symbol_families: symbol_total,
            raw_symbol_matches: raw_symbol_total,
            preview_hits: hits.len(),
            returned_hits: hit_total,
            indexed_artifacts: response.indexed_artifacts,
            skipped_artifacts: response.skipped_artifacts,
            max_items,
            max_bytes,
        },
        narrow_commands: dedupe_preserve_order(narrow_commands),
    })
}

pub(crate) fn build_search_budget_report(input: SearchBudgetReportInput<'_>) -> SearchBudgetReport {
    let SearchBudgetReportInput {
        query,
        strategy,
        root,
        response,
        symbol_hits,
        absolute,
        budget,
        filters,
    } = input;
    let max_items = budget.preview_items();
    let max_bytes = budget.preview_bytes();
    let raw_symbol_total = symbol_hits.len();
    let hit_total = response.hits.len();
    let mut family_positions = HashMap::new();
    let mut families = Vec::new();
    let summary_db = search_budget_summary_db(root);

    for hit in symbol_hits {
        let display_file = if absolute {
            hit.file.clone()
        } else {
            relativize(&hit.file, root)
        };
        let mut representative_hit = hit.clone();
        representative_hit.file = display_file.clone();
        let canonical_family = canonical_tag_family_from_symbol(&hit.name, hit.tags.as_deref());
        let family_key = canonical_family
            .as_ref()
            .map(|family| family.canonical.clone())
            .unwrap_or_else(|| hit.name.clone());
        let position = *family_positions.entry(family_key).or_insert_with(|| {
            families.push(SearchBudgetSymbolFamily {
                canonical_family: canonical_family
                    .as_ref()
                    .map(|family| family.canonical.clone()),
                canonical_tag_alias: canonical_family
                    .as_ref()
                    .map(|family| family.tag_alias.clone()),
                representative_name: hit.name.clone(),
                representative_kind: hit.kind.clone(),
                representative_match_type: hit.match_type.clone(),
                representative_hit,
                representative_file: display_file.clone(),
                representative_line: hit.line,
                representative_score: hit.score,
                seen_surfaces: HashSet::new(),
                seen_files: HashSet::new(),
                surface_examples: Vec::new(),
                match_count: 0,
            });
            families.len() - 1
        });

        let family = &mut families[position];
        family.match_count += 1;
        if family.seen_surfaces.insert(hit.name.clone())
            && family.surface_examples.len() < SEARCH_BUDGET_SURFACE_PREVIEW_LIMIT
        {
            family
                .surface_examples
                .push(truncate_for_budget(&hit.name, max_bytes));
        }
        family.seen_files.insert(display_file);
    }

    let symbol_total = families.len();
    let symbols: Vec<SearchBudgetSymbolPreview> = families
        .into_iter()
        .take(max_items)
        .map(|family| {
            let file_count = family.seen_files.len();
            let surface_count = family.seen_surfaces.len();
            let key = format!(
                "{}:{}:{}:{}:{}:{}:{}",
                family
                    .canonical_family
                    .as_deref()
                    .or(family.canonical_tag_alias.as_deref())
                    .unwrap_or(&family.representative_name),
                family.canonical_tag_alias.as_deref().unwrap_or(""),
                family.representative_kind,
                family.representative_file,
                family.representative_line,
                query,
                strategy
            );
            let ast = search_budget_ast_artifact(root, &family.representative_hit);
            let summary_refs = search_budget_summary_ref_count(
                summary_db.as_ref(),
                root,
                &family.representative_hit,
            );
            let graph_neighbors = search_budget_graph_neighbor_count(ast.as_ref());
            SearchBudgetSymbolPreview {
                handle: stable_handle("sfam", &key),
                tag_alias: family
                    .canonical_tag_alias
                    .as_deref()
                    .map(|alias| truncate_for_budget(alias, max_bytes)),
                match_type: family.representative_match_type,
                kind: family.representative_kind,
                language: family.representative_hit.language,
                name: format_search_budget_symbol_name(
                    &family.representative_name,
                    surface_count,
                    max_bytes,
                ),
                file: format_search_budget_symbol_file(
                    &family.representative_file,
                    file_count,
                    max_bytes,
                ),
                line: family.representative_line,
                end_line: family.representative_hit.end_line,
                score: family.representative_score,
                match_count: family.match_count,
                surface_count,
                file_count,
                summary_refs,
                graph_neighbors,
                surface_examples: family.surface_examples,
                ast,
                expand: build_search_budget_family_expand(
                    strategy,
                    root.to_string_lossy().as_ref(),
                    family.canonical_tag_alias.as_deref(),
                    &family.representative_name,
                ),
            }
        })
        .collect();

    let hits: Vec<SearchBudgetHitPreview> = response
        .hits
        .iter()
        .take(max_items)
        .map(|hit| {
            let display_path = if absolute {
                hit.path.clone()
            } else {
                relativize(&hit.path, root)
            };
            let key = format!("{}:{}:{}:{}", display_path, hit.rank, hit.score, query);
            let preview = compact_snippet(&hit.snippet)
                .map(|snippet| truncate_for_budget(&snippet, max_bytes))
                .unwrap_or_default();
            SearchBudgetHitPreview {
                handle: stable_handle("shit", &key),
                rank: hit.rank,
                path: truncate_for_budget(&display_path, max_bytes),
                confidence: format!("{:?}", hit.confidence),
                score: hit.score,
                preview,
                expand: build_search_budget_follow_up(query, strategy, &display_path),
            }
        })
        .collect();

    let ranking = search_budget_ranking_profile();
    let ranked = build_search_budget_ranked_previews(query, max_items, max_bytes, &symbols, &hits);
    let scale_guard = build_search_scale_guard(
        query,
        strategy,
        root,
        response,
        symbol_total,
        raw_symbol_total,
        hit_total,
        max_items,
        max_bytes,
        &symbols,
        &hits,
    );

    SearchBudgetReport {
        query: query.to_string(),
        strategy: strategy.to_string(),
        filters: filters.clone(),
        indexed_artifacts: response.indexed_artifacts,
        skipped_artifacts: response.skipped_artifacts,
        max_items,
        max_bytes,
        symbol_total,
        raw_symbol_total,
        hit_total,
        truncated: symbol_total > max_items || hit_total > max_items,
        scale_guard,
        symbols,
        hits,
        ranking,
        ranked,
    }
}

fn append_search_facet_filter_summary(parts: &mut Vec<String>, name: &str, values: &[String]) {
    if !values.is_empty() {
        parts.push(format!("{name}={}", values.join("|")));
    }
}

fn append_search_facet_usize_filter_summary(parts: &mut Vec<String>, name: &str, values: &[usize]) {
    if !values.is_empty() {
        parts.push(format!(
            "{name}={}",
            values
                .iter()
                .map(usize::to_string)
                .collect::<Vec<_>>()
                .join("|")
        ));
    }
}

pub(crate) fn search_facet_filters_summary(filters: &SearchFacetFilters) -> String {
    let mut parts = Vec::new();
    append_search_facet_filter_summary(&mut parts, "lang", &filters.languages);
    append_search_facet_filter_summary(&mut parts, "kind", &filters.kinds);
    append_search_facet_filter_summary(&mut parts, "node-kind", &filters.node_kinds);
    append_search_facet_filter_summary(&mut parts, "section", &filters.sections);
    append_search_facet_filter_summary(&mut parts, "parent", &filters.parents);
    append_search_facet_filter_summary(&mut parts, "child", &filters.children);
    append_search_facet_filter_summary(&mut parts, "fence-language", &filters.fence_languages);
    append_search_facet_usize_filter_summary(&mut parts, "list-depth", &filters.list_depths);
    append_search_facet_usize_filter_summary(&mut parts, "heading-level", &filters.heading_levels);
    parts.join(" ")
}

pub(crate) fn print_search_budget_human(report: &SearchBudgetReport) {
    println!(
        "search-budget q:{} strategy:{} symbols:{}/{} raw-symbols:{} hits:{}/{} indexed:{} skipped:{}",
        shell_quote(&report.query),
        report.strategy,
        report.symbols.len(),
        report.symbol_total,
        report.raw_symbol_total,
        report.hits.len(),
        report.hit_total,
        report.indexed_artifacts,
        report.skipped_artifacts
    );
    if !report.filters.is_empty() {
        println!("filters: {}", search_facet_filters_summary(&report.filters));
    }
    if !report.ranked.is_empty() {
        println!(
            "ranking: {} symbol-span-weight:{} lexical-file-weight:{} summary-boost:{} graph-boost:{}",
            report.ranking.mode,
            format_score(report.ranking.symbol_span_weight, true),
            format_score(report.ranking.lexical_file_weight, true),
            format_score(report.ranking.summary_boost, true),
            format_score(report.ranking.graph_boost, true)
        );
    }
    for item in &report.ranked {
        let label = item
            .name
            .as_deref()
            .map(|name| format!(" {name}"))
            .unwrap_or_default();
        let line = item.line.map(|line| format!(":{line}")).unwrap_or_default();
        let reasons = if item.reasons.is_empty() {
            String::new()
        } else {
            format!(" reasons:{}", item.reasons.join(","))
        };
        println!(
            "rank {} #{} [{} {}] {}{}{}{} expand:{}",
            item.handle,
            item.rank,
            item.source,
            format_score(item.score, true),
            item.path,
            line,
            label,
            reasons,
            item.expand
        );
    }
    for symbol in &report.symbols {
        let variants = if symbol.surface_examples.is_empty() {
            String::new()
        } else {
            format!(" variants:{}", symbol.surface_examples.join(", "))
        };
        println!(
            "sym {} [{}] {} {}:{} sc:{} matches:{} files:{}{} expand:{}",
            format_symbol_preview_line(&symbol.handle, &symbol.name, symbol.tag_alias.as_deref()),
            symbol.match_type,
            symbol.kind,
            symbol.file,
            symbol.line,
            format_score(symbol.score, true),
            symbol.match_count,
            symbol.file_count,
            variants,
            symbol.expand
        );
        if symbol.summary_refs > 0 || symbol.graph_neighbors > 0 {
            println!(
                "  evidence summary_refs:{} graph_neighbors:{}",
                symbol.summary_refs, symbol.graph_neighbors
            );
        }
        if let Some(ast) = &symbol.ast {
            let markdown_ast = ast
                .expand
                .markdown_ast
                .as_ref()
                .map(|command| format!(" markdown-ast:{command}"))
                .unwrap_or_default();
            println!(
                "  ast {} {}:{}-{} bytes:{}-{} source:{} symbol:{}{}",
                ast.span.handle,
                ast.span.node_kind,
                ast.span.start_line,
                ast.span.end_line,
                ast.span.start_byte,
                ast.span.end_byte,
                ast.expand.source_window,
                ast.expand.symbol_read,
                markdown_ast
            );
        }
    }
    for hit in &report.hits {
        if hit.preview.is_empty() {
            println!(
                "hit {} #{} {} [{} {}] expand:{}",
                hit.handle,
                hit.rank,
                hit.path,
                hit.confidence,
                format_score(hit.score, true),
                hit.expand
            );
        } else {
            println!(
                "hit {} #{} {} [{} {}] {} expand:{}",
                hit.handle,
                hit.rank,
                hit.path,
                hit.confidence,
                format_score(hit.score, true),
                hit.preview,
                hit.expand
            );
        }
    }
    if report.truncated {
        println!(
            "budget truncated items:{} bytes:{}",
            report.max_items, report.max_bytes
        );
    }
    if let Some(guard) = &report.scale_guard {
        println!("scale guard [{}]: {}", guard.level, guard.warning);
        println!(
            "signals preview-symbols:{} symbol-families:{} raw-symbols:{} preview-hits:{} hits:{} indexed:{} skipped:{} budget-items:{} budget-bytes:{}",
            guard.signals.preview_symbols,
            guard.signals.symbol_families,
            guard.signals.raw_symbol_matches,
            guard.signals.preview_hits,
            guard.signals.returned_hits,
            guard.signals.indexed_artifacts,
            guard.signals.skipped_artifacts,
            guard.signals.max_items,
            guard.signals.max_bytes
        );
        for command in &guard.narrow_commands {
            println!("narrow: {command}");
        }
    }
}
