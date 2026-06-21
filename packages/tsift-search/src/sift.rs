use std::cmp::Ordering;
use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use ignore::WalkBuilder;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum AcquisitionAdapterKind {
    FileSystem,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ContextArtifactKind {
    File,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub enum ScoreConfidence {
    High,
    Medium,
    Low,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum SearchCoverageMode {
    Sealed,
    Converging,
    Frontier,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ArtifactBudget {
    pub bytes: usize,
    pub segment_count: usize,
    pub token_estimate: usize,
}

impl ArtifactBudget {
    pub fn from_text(text: &str, segment_count: usize) -> Self {
        let bytes = text.len();
        Self {
            bytes,
            segment_count,
            token_estimate: (bytes / 8).max(1),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ArtifactFreshness {
    pub modified_unix_secs: Option<i64>,
    pub observed_unix_secs: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ArtifactProvenance {
    pub adapter: AcquisitionAdapterKind,
    pub source: String,
    pub synthetic: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct SearchHit {
    pub artifact_id: String,
    pub artifact_kind: ContextArtifactKind,
    pub budget: ArtifactBudget,
    pub confidence: ScoreConfidence,
    pub freshness: ArtifactFreshness,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub location: Option<String>,
    pub path: String,
    pub provenance: ArtifactProvenance,
    pub rank: usize,
    pub score: f64,
    pub snippet: String,
}

impl SearchHit {
    pub fn to_terse(&self) -> TerseSearchHit {
        TerseSearchHit {
            artifact_id: self.artifact_id.clone(),
            confidence: format!("{:?}", self.confidence).to_lowercase(),
            location: self.location.clone(),
            path: self.path.clone(),
            rank: self.rank,
            score: self.score,
            snippet: self.snippet.clone(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct TerseSearchHit {
    pub artifact_id: String,
    pub confidence: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub location: Option<String>,
    pub path: String,
    pub rank: usize,
    pub score: f64,
    pub snippet: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SearchCoverageSnapshot {
    pub active_rebuild: Option<String>,
    pub completed_dirty_sector_count: usize,
    pub dirty_sector_count: usize,
    pub mode: SearchCoverageMode,
    pub mounted_sector_count: usize,
    pub rebuilding_sector_count: usize,
    pub resumed_sector_count: usize,
    pub reused_sector_count: usize,
    pub total_sector_count: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct SearchResponse {
    pub coverage: SearchCoverageSnapshot,
    pub hits: Vec<SearchHit>,
    pub indexed_artifacts: usize,
    pub root: String,
    pub skipped_artifacts: usize,
    pub strategy: String,
}

#[derive(Debug, Clone)]
pub struct SearchOptions {
    limit: usize,
    strategy: String,
}

impl Default for SearchOptions {
    fn default() -> Self {
        Self {
            limit: 10,
            strategy: "lexical".to_string(),
        }
    }
}

impl SearchOptions {
    pub fn with_limit(mut self, limit: usize) -> Self {
        self.limit = limit;
        self
    }

    pub fn with_strategy(mut self, strategy: String) -> Self {
        self.strategy = strategy;
        self
    }
}

#[derive(Debug, Clone)]
pub struct SearchInput {
    root: PathBuf,
    query: String,
    options: SearchOptions,
}

impl SearchInput {
    pub fn new(root: &Path, query: &str) -> Self {
        Self {
            root: root.to_path_buf(),
            query: query.to_string(),
            options: SearchOptions::default(),
        }
    }

    pub fn with_options(mut self, options: SearchOptions) -> Self {
        self.options = options;
        self
    }
}

/// In-memory lexical inversion used **only** as a live fallback for the FTS5
/// `index.db` path (degraded/stale index held by a concurrent writer,
/// `--no-autoindex` on an un-indexed root, or direct programmatic callers).
///
/// #015t Phase 4b: the JSON persistence (`token-index.json`) was deleted. That
/// cache was keyed on file *existence* only — `load_or_build_token_index`
/// returned it whenever the file merely existed, with no mtime/content
/// invalidation, so once written it served stale matches forever (files added
/// or modified afterward were silently missing). Because every site that
/// reaches this type needs *live* results, caching it to disk was wrong by
/// construction; the fallback now always rebuilds in-memory.
///
/// #015t Phase 4b(a) decision (operator, 2026-06-20): **keep this in-memory
/// rebuild** as the degraded-read-only fallback rather than replacing the call
/// site with a literal `rg -F` walk and deleting the type. Both are equally
/// *live*, so the choice is not about freshness — it is about parity. This
/// rebuild preserves the tokenized **OR-union** matching and token-overlap
/// ranking of the FTS path (`fts_match_query` / `content_fts`), so the
/// transient degraded window stays behaviorally indistinguishable from the
/// healthy path. `rg -F` would silently switch the fallback to literal
/// substring matching with no ranking — a hard-to-debug divergence in a path
/// that is meant to be an invisible safety net. Do not "simplify" by deleting
/// `TokenIndex` without re-opening that decision.
#[derive(Debug, Clone, Default)]
pub struct TokenIndex {
    token_to_files: HashMap<String, Vec<String>>,
    total_files: usize,
}

impl TokenIndex {
    pub fn build(root: &Path) -> Result<Self> {
        let mut token_to_files: HashMap<String, Vec<String>> = HashMap::new();
        let mut total_files = 0usize;
        for path in candidate_files(root)? {
            let Ok(contents) = fs::read_to_string(&path) else {
                continue;
            };
            total_files += 1;
            let path_str = path.display().to_string();
            let mut seen = HashSet::new();
            for line in contents.lines() {
                for token in tokenize_iter(line) {
                    if seen.insert(token.clone()) {
                        token_to_files
                            .entry(token)
                            .or_default()
                            .push(path_str.clone());
                    }
                }
            }
        }
        Ok(Self {
            token_to_files,
            total_files,
        })
    }

    pub fn files_matching_any(&self, tokens: &[String]) -> HashSet<PathBuf> {
        let mut files = HashSet::new();
        for token in tokens {
            if let Some(paths) = self.token_to_files.get(token) {
                for p in paths {
                    files.insert(PathBuf::from(p));
                }
            }
        }
        files
    }

    pub fn total_files(&self) -> usize {
        self.total_files
    }

    pub fn unique_tokens(&self) -> usize {
        self.token_to_files.len()
    }
}

#[derive(Debug, Clone, Default)]
pub struct SiftBuilder {
    cache_dir: Option<PathBuf>,
}

impl SiftBuilder {
    pub fn with_cache_dir(mut self, cache_dir: &Path) -> Self {
        self.cache_dir = Some(cache_dir.to_path_buf());
        self
    }

    pub fn build(self) -> Sift {
        Sift {
            cache_dir: self.cache_dir,
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct Sift {
    cache_dir: Option<PathBuf>,
}

impl Sift {
    pub fn builder() -> SiftBuilder {
        SiftBuilder::default()
    }

    pub fn search(&self, input: SearchInput) -> Result<SearchResponse> {
        if let Some(cache_dir) = &self.cache_dir {
            fs::create_dir_all(cache_dir).with_context(|| {
                format!("creating search cache directory: {}", cache_dir.display())
            })?;
        }

        if input.options.limit == 0 {
            return Ok(SearchResponse {
                coverage: sealed_coverage(0),
                hits: Vec::new(),
                indexed_artifacts: 0,
                root: input.root.display().to_string(),
                skipped_artifacts: 0,
                strategy: input.options.strategy,
            });
        }

        let query_tokens = tokenize(&input.query);
        // #015t Phase 4b: always build the inversion live. This path is only
        // reached as a fallback that must return current results, so it must not
        // read a persisted `token-index.json` (the deleted, never-invalidated
        // cache). The generic `cache_dir` hook above is retained for a future,
        // properly-invalidated cache but no longer backs the token index.
        let token_index = TokenIndex::build(&input.root)?;

        let filtered_files = if query_tokens.is_empty() {
            candidate_files(&input.root)?
        } else {
            token_index
                .files_matching_any(&query_tokens)
                .into_iter()
                .filter(|p| p.exists())
                .collect()
        };

        let mut candidates = Vec::new();
        let mut indexed_artifacts = 0usize;
        let mut skipped_artifacts = 0usize;
        for path in filtered_files {
            let Ok(contents) = fs::read_to_string(&path) else {
                skipped_artifacts += 1;
                continue;
            };
            indexed_artifacts += 1;
            if let Some(candidate) = score_file(&path, &contents, &input.query, &query_tokens) {
                candidates.push(candidate);
            }
        }

        candidates.sort_by(|left, right| {
            right
                .score
                .partial_cmp(&left.score)
                .unwrap_or(Ordering::Equal)
                .then_with(|| left.path.cmp(&right.path))
                .then_with(|| left.line.cmp(&right.line))
        });

        let hits = candidates
            .into_iter()
            .take(input.options.limit)
            .enumerate()
            .map(|(idx, candidate)| candidate.into_hit(idx + 1))
            .collect();

        Ok(SearchResponse {
            coverage: sealed_coverage(indexed_artifacts),
            hits,
            indexed_artifacts,
            root: input.root.display().to_string(),
            skipped_artifacts,
            strategy: input.options.strategy,
        })
    }

}

#[derive(Debug)]
struct FileCandidate {
    path: PathBuf,
    line: usize,
    score: f64,
    snippet: String,
}

impl FileCandidate {
    fn into_hit(self, rank: usize) -> SearchHit {
        let path = self.path.display().to_string();
        SearchHit {
            artifact_id: format!("lexical:{}:{}:{}", path, self.line, rank),
            artifact_kind: ContextArtifactKind::File,
            budget: ArtifactBudget::from_text(&self.snippet, 1),
            confidence: confidence_for_score(self.score),
            freshness: file_timestamp(&self.path),
            location: Some(format!("line {}", self.line)),
            path,
            provenance: ArtifactProvenance {
                adapter: AcquisitionAdapterKind::FileSystem,
                source: "tsift local lexical adapter".to_string(),
                synthetic: false,
            },
            rank,
            score: self.score,
            snippet: self.snippet,
        }
    }
}

fn sealed_coverage(indexed_artifacts: usize) -> SearchCoverageSnapshot {
    SearchCoverageSnapshot {
        active_rebuild: None,
        completed_dirty_sector_count: 0,
        dirty_sector_count: 0,
        mode: SearchCoverageMode::Sealed,
        mounted_sector_count: indexed_artifacts,
        rebuilding_sector_count: 0,
        resumed_sector_count: 0,
        reused_sector_count: 0,
        total_sector_count: indexed_artifacts,
    }
}

fn candidate_files(root: &Path) -> Result<Vec<PathBuf>> {
    if root.is_file() {
        return Ok(vec![root.to_path_buf()]);
    }

    let mut files = Vec::new();
    for entry in WalkBuilder::new(root)
        .hidden(true)
        .git_ignore(true)
        .git_exclude(true)
        .build()
    {
        let entry = entry.with_context(|| format!("walking search root: {}", root.display()))?;
        if entry
            .file_type()
            .is_some_and(|file_type| file_type.is_file())
        {
            files.push(entry.path().to_path_buf());
        }
    }
    Ok(files)
}

fn tokenize(value: &str) -> Vec<String> {
    value
        .split(|ch: char| !ch.is_alphanumeric())
        .filter(|token| !token.is_empty())
        .map(|token| token.to_lowercase())
        .collect()
}

fn tokenize_iter(value: &str) -> impl Iterator<Item = String> + '_ {
    value
        .split(|ch: char| !ch.is_alphanumeric())
        .filter(|token| !token.is_empty())
        .map(|token| token.to_lowercase())
}

/// Translate a free-text query into an FTS5 `MATCH` expression with the **same
/// candidate semantics as the [`TokenIndex`]** (#015t Phase 3). FTS5 treats a bare
/// multi-token string as an adjacency *phrase*, but `TokenIndex::files_matching_any`
/// is an **OR-union** over the query tokens — so the FTS path must OR the tokens to
/// return a superset of the TokenIndex candidate set (the Phase 3 soundness gate).
/// Each token is double-quoted (with `"` escaped) so identifiers can't be parsed as
/// FTS5 operators. Returns `None` when the query has no indexable tokens.
pub fn fts_match_query(query: &str) -> Option<String> {
    let tokens = tokenize(query);
    if tokens.is_empty() {
        return None;
    }
    Some(
        tokens
            .iter()
            .map(|token| format!("\"{}\"", token.replace('"', "\"\"")))
            .collect::<Vec<_>>()
            .join(" OR "),
    )
}

/// #015t Phase 3b — flag-gated `index.db`-backed search. Translates the query to
/// an FTS5 `MATCH` (preserving the `TokenIndex` OR-union candidate semantics via
/// [`fts_match_query`]), runs the BM25-ranked content search, and builds a
/// [`SearchResponse`] whose **file ordering is BM25** and whose per-file line +
/// snippet is chosen by the **same substring/token line scorer as the lexical
/// path** ([`score_file`]) — the plan's "BM25-vs-substring top-K reconciliation".
/// Each hit's body is read from the inline FTS column, so no file is re-read from
/// disk. The JSON `TokenIndex` stays the default search path; this runs only when
/// the caller opts in (`TSIFT_FTS_SEARCH`) and a real `index.db` exists.
pub fn fts_search(
    db_path: &Path,
    root: &Path,
    query: &str,
    limit: usize,
) -> Result<SearchResponse> {
    let empty = |indexed: usize| SearchResponse {
        coverage: sealed_coverage(indexed),
        hits: Vec::new(),
        indexed_artifacts: indexed,
        root: root.display().to_string(),
        skipped_artifacts: 0,
        strategy: "fts".to_string(),
    };

    if limit == 0 {
        return Ok(empty(0));
    }
    let Some(fts_query) = fts_match_query(query) else {
        return Ok(empty(0));
    };

    let db = tsift_index::index::IndexDb::open_read_only_resilient(db_path)
        .with_context(|| format!("opening index db for FTS search: {}", db_path.display()))?;
    let raw_hits = db.content_fts_search_with_body(&fts_query, limit)?;
    let query_tokens = tokenize(query);
    let indexed_artifacts = raw_hits.len();

    let hits = raw_hits
        .into_iter()
        .enumerate()
        .map(|(idx, (path, _bm25, body))| {
            let rank = idx + 1;
            // BM25 (from `content_fts_search_with_body`) owns the file ranking;
            // the substring/token scorer only picks the representative line +
            // snippet within the matched body. When folding/tokenization matched
            // a file no substring line covers, fall back to its first real line.
            let (line, snippet) = score_file(Path::new(&path), &body, query, &query_tokens)
                .map(|candidate| (candidate.line, candidate.snippet))
                .unwrap_or_else(|| representative_line(&body));
            // Descending score preserves BM25 order through any later merge step
            // (mirrors the exact-search path's rank-derived score).
            let score = (limit.saturating_sub(rank).saturating_add(1)) as f64;
            SearchHit {
                artifact_id: format!("fts:{}:{}:{}", path, line, rank),
                artifact_kind: ContextArtifactKind::File,
                budget: ArtifactBudget::from_text(&snippet, 1),
                confidence: ScoreConfidence::High,
                freshness: file_timestamp(Path::new(&path)),
                location: Some(format!("line {}", line)),
                path,
                provenance: ArtifactProvenance {
                    adapter: AcquisitionAdapterKind::FileSystem,
                    source: "tsift index.db FTS5 adapter".to_string(),
                    synthetic: false,
                },
                rank,
                score,
                snippet,
            }
        })
        .collect();

    Ok(SearchResponse {
        coverage: sealed_coverage(indexed_artifacts),
        hits,
        indexed_artifacts,
        root: root.display().to_string(),
        skipped_artifacts: 0,
        strategy: "fts".to_string(),
    })
}

/// First non-empty line (1-based) of `body` and its trimmed text, used as the FTS
/// snippet fallback when the substring/token scorer finds no covering line.
fn representative_line(body: &str) -> (usize, String) {
    for (idx, line) in body.lines().enumerate() {
        if !line.trim().is_empty() {
            return (idx + 1, line.trim().to_string());
        }
    }
    (1, String::new())
}

fn score_file(
    path: &Path,
    contents: &str,
    query: &str,
    query_tokens: &[String],
) -> Option<FileCandidate> {
    let query_lower = query.to_lowercase();
    let mut best: Option<FileCandidate> = None;
    for (idx, line) in contents.lines().enumerate() {
        let line_lower = line.to_lowercase();
        let phrase_score = if !query_lower.is_empty() && line_lower.contains(&query_lower) {
            50.0 + query_lower.len() as f64
        } else {
            0.0
        };
        let token_score = if query_tokens.is_empty() {
            0.0
        } else {
            query_tokens
                .iter()
                .filter(|token| line_lower.contains(token.as_str()))
                .count() as f64
                * 10.0
        };
        let score = phrase_score + token_score;
        if score <= 0.0 {
            continue;
        }
        let candidate = FileCandidate {
            path: path.to_path_buf(),
            line: idx + 1,
            score,
            snippet: line.trim().to_string(),
        };
        if best
            .as_ref()
            .is_none_or(|current| candidate.score > current.score)
        {
            best = Some(candidate);
        }
    }
    best
}

fn confidence_for_score(score: f64) -> ScoreConfidence {
    if score >= 60.0 {
        ScoreConfidence::High
    } else if score >= 20.0 {
        ScoreConfidence::Medium
    } else {
        ScoreConfidence::Low
    }
}

fn file_timestamp(path: &Path) -> ArtifactFreshness {
    let observed_unix_secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64;
    let modified_unix_secs = fs::metadata(path)
        .ok()
        .and_then(|metadata| metadata.modified().ok())
        .and_then(|modified| modified.duration_since(UNIX_EPOCH).ok())
        .map(|duration| duration.as_secs() as i64);
    ArtifactFreshness {
        modified_unix_secs,
        observed_unix_secs,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn local_adapter_returns_ranked_lexical_hits() {
        let dir = tempfile::tempdir().unwrap();
        let alpha = dir.path().join("alpha.rs");
        let beta = dir.path().join("beta.rs");
        fs::write(&alpha, "fn unrelated() {}\n").unwrap();
        fs::write(&beta, "fn route_dispatch() {}\n").unwrap();

        let response = Sift::builder()
            .with_cache_dir(&dir.path().join(".tsift/search-cache"))
            .build()
            .search(SearchInput::new(dir.path(), "route dispatch"))
            .unwrap();

        assert_eq!(response.strategy, "lexical");
        assert_eq!(response.indexed_artifacts, 1);
        assert_eq!(response.hits.len(), 1);
        assert!(response.hits[0].path.ends_with("beta.rs"));
        assert_eq!(response.hits[0].location.as_deref(), Some("line 1"));
        assert_eq!(
            response.hits[0].provenance.source,
            "tsift local lexical adapter"
        );
    }

    #[test]
    fn local_adapter_creates_cache_dir_and_serializes_stable_shapes() {
        let dir = tempfile::tempdir().unwrap();
        let cache_dir = dir.path().join("cache");
        fs::write(dir.path().join("main.rs"), "fn main() {}\n").unwrap();

        let response = Sift::builder()
            .with_cache_dir(&cache_dir)
            .build()
            .search(SearchInput::new(dir.path(), "main"))
            .unwrap();
        let json = serde_json::to_value(&response).unwrap();

        assert!(cache_dir.exists());
        assert_eq!(json["coverage"]["mode"], "sealed");
        assert_eq!(json["hits"][0]["artifact_kind"], "file");
        assert_eq!(json["hits"][0]["confidence"], "High");
        assert_eq!(json["hits"][0]["provenance"]["adapter"], "file-system");
    }

    #[test]
    fn terse_search_hit_strips_budget_freshness_provenance() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("main.rs"), "fn main() {}\n").unwrap();
        let response = Sift::builder()
            .build()
            .search(SearchInput::new(dir.path(), "main"))
            .unwrap();
        let hit = &response.hits[0];
        let terse = hit.to_terse();
        assert_eq!(terse.artifact_id, hit.artifact_id);
        assert_eq!(terse.path, hit.path);
        assert_eq!(terse.rank, hit.rank);
        let terse_json = serde_json::to_string(&terse).unwrap();
        let full_json = serde_json::to_string(hit).unwrap();
        assert!(
            terse_json.len() < full_json.len(),
            "terse ({}) should be shorter than full ({})",
            terse_json.len(),
            full_json.len()
        );
        assert!(!terse_json.contains("budget"));
        assert!(!terse_json.contains("freshness"));
        assert!(!terse_json.contains("provenance"));
    }

    #[test]
    fn token_index_build_and_query() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("alpha.rs"), "fn route_dispatch() {}\n").unwrap();
        fs::write(dir.path().join("beta.rs"), "fn unrelated() {}\n").unwrap();

        let index = TokenIndex::build(dir.path()).unwrap();
        assert_eq!(index.total_files(), 2);
        assert!(index.unique_tokens() > 0);

        let matching = index.files_matching_any(&["route".to_string()]);
        assert_eq!(matching.len(), 1);
        assert!(matching.iter().any(|p| p.ends_with("alpha.rs")));
    }

    #[test]
    fn token_index_skips_files_with_no_matching_tokens() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("a.rs"), "fn target_function() {}\n").unwrap();
        fs::write(dir.path().join("b.rs"), "fn completely_different() {}\n").unwrap();
        fs::write(dir.path().join("c.rs"), "struct Widget;\n").unwrap();

        let index = TokenIndex::build(dir.path()).unwrap();
        let matching = index.files_matching_any(&["target".to_string()]);
        assert_eq!(matching.len(), 1);
        assert!(matching.iter().any(|p| p.ends_with("a.rs")));
    }

    #[test]
    fn token_index_multi_token_union() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("a.rs"), "fn alpha() {}\n").unwrap();
        fs::write(dir.path().join("b.rs"), "fn beta() {}\n").unwrap();
        fs::write(dir.path().join("c.rs"), "fn gamma() {}\n").unwrap();

        let index = TokenIndex::build(dir.path()).unwrap();
        let matching = index.files_matching_any(&["alpha".to_string(), "gamma".to_string()]);
        assert_eq!(matching.len(), 2);
    }

    #[test]
    fn token_index_no_match_returns_empty() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("a.rs"), "fn foo() {}\n").unwrap();

        let index = TokenIndex::build(dir.path()).unwrap();
        let matching = index.files_matching_any(&["nonexistent".to_string()]);
        assert!(matching.is_empty());
    }

    #[test]
    fn search_uses_token_index_to_skip_unrelated_files() {
        let dir = tempfile::tempdir().unwrap();
        let cache_dir = dir.path().join("cache");
        fs::write(dir.path().join("target.rs"), "fn target_function() {}\n").unwrap();
        fs::write(dir.path().join("noise.rs"), "fn unrelated_stuff() {}\n").unwrap();
        fs::write(dir.path().join("other.rs"), "struct Placeholder;\n").unwrap();

        let response = Sift::builder()
            .with_cache_dir(&cache_dir)
            .build()
            .search(SearchInput::new(dir.path(), "target_function"))
            .unwrap();

        assert_eq!(response.hits.len(), 1);
        assert!(response.hits[0].path.ends_with("target.rs"));
        // #015t Phase 4b: the never-invalidated `token-index.json` cache is gone —
        // the fallback builds live and persists nothing.
        assert!(
            !cache_dir.join("token-index.json").exists(),
            "token-index.json persistence must be deleted"
        );
    }

    #[test]
    fn search_reflects_files_added_after_first_search() {
        // #015t Phase 4b regression: the old `token-index.json` cache was keyed on
        // file existence only — once written it was returned forever, so a file
        // created after the first search was invisible to the lexical fallback.
        // The fallback now rebuilds live on every call, so the new file is found.
        let dir = tempfile::tempdir().unwrap();
        let cache_dir = dir.path().join("cache");
        fs::write(dir.path().join("first.rs"), "fn shared_token() {}\n").unwrap();

        let engine = Sift::builder().with_cache_dir(&cache_dir).build();
        let first = engine
            .search(SearchInput::new(dir.path(), "shared_token"))
            .unwrap();
        assert_eq!(first.hits.len(), 1);
        assert!(first.hits[0].path.ends_with("first.rs"));

        // Add a second file with the same token AFTER the first search ran.
        fs::write(dir.path().join("second.rs"), "fn shared_token() {}\n").unwrap();

        let second = engine
            .search(SearchInput::new(dir.path(), "shared_token"))
            .unwrap();
        let paths: Vec<_> = second.hits.iter().map(|h| h.path.clone()).collect();
        assert_eq!(
            second.hits.len(),
            2,
            "live fallback must see the newly added file: {paths:?}"
        );
        assert!(paths.iter().any(|p| p.ends_with("second.rs")));
    }

    #[test]
    fn token_index_empty_query_returns_all() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("a.rs"), "fn foo() {}\n").unwrap();

        let response = Sift::builder()
            .build()
            .search(SearchInput::new(dir.path(), ""))
            .unwrap();

        assert_eq!(response.indexed_artifacts, 1);
        assert_eq!(response.hits.len(), 0);
    }

    #[test]
    fn fts_match_query_ors_tokens() {
        // Mirrors TokenIndex OR-union semantics, not an FTS phrase.
        assert_eq!(fts_match_query("beta_call").as_deref(), Some("\"beta\" OR \"call\""));
        assert_eq!(fts_match_query("Foo").as_deref(), Some("\"foo\""));
        assert_eq!(fts_match_query("   ").as_deref(), None);
    }

    #[test]
    fn fts_content_index_supersets_token_index_candidates() {
        // Phase 3 soundness gate: the index.db FTS path must return every file the
        // authoritative TokenIndex treats as a candidate (FTS ⊇ relevance set), so a
        // future flagged cutover cannot silently drop a lexical hit. BM25 vs substring
        // *ranking* may differ; candidate *coverage* may not. Uses fts_match_query to
        // preserve the TokenIndex OR-union semantics.
        use tsift_index::index::IndexDb;

        let src = tempfile::tempdir().unwrap();
        let root = src.path();
        fs::write(root.join("alpha.rs"), "fn alpha_handler() { beta_call(); }\n").unwrap();
        fs::write(root.join("beta.rs"), "fn beta_call() { gamma(); }\n").unwrap();
        fs::write(root.join("noise.rs"), "fn unrelated_thing() {}\n").unwrap();

        let basenames = |paths: HashSet<PathBuf>| -> HashSet<String> {
            paths
                .into_iter()
                .filter_map(|p| p.file_name().map(|n| n.to_string_lossy().into_owned()))
                .collect()
        };

        let query = "beta_call";
        let token_index = TokenIndex::build(root).unwrap();
        let ti_candidates = basenames(token_index.files_matching_any(&tokenize(query)));
        assert!(!ti_candidates.is_empty(), "fixture should yield TokenIndex candidates");

        // Build the index.db in a separate dir so it does not index itself.
        let db_dir = tempfile::tempdir().unwrap();
        let db = IndexDb::open(&db_dir.path().join("index.db")).unwrap();
        db.apply_changes(root).unwrap();
        let fts_query = fts_match_query(query).expect("query has tokens");
        let fts_hits: HashSet<String> = db
            .content_fts_search(&fts_query, 100)
            .unwrap()
            .into_iter()
            .filter_map(|(p, _)| Path::new(&p).file_name().map(|n| n.to_string_lossy().into_owned()))
            .collect();

        for candidate in &ti_candidates {
            assert!(
                fts_hits.contains(candidate),
                "FTS result set missing TokenIndex candidate {candidate}: fts={fts_hits:?}"
            );
        }
    }

    #[test]
    fn fts_search_builds_search_response_from_index_db() {
        // Phase 3b: fts_search turns the BM25 content hits into a SearchResponse
        // with per-file line + snippet picked by the lexical scorer, strategy
        // "fts", and BM25-preserving descending scores.
        use tsift_index::index::IndexDb;

        let src = tempfile::tempdir().unwrap();
        let root = src.path();
        fs::write(
            root.join("alpha.rs"),
            "fn unrelated() {}\nfn alpha_handler() { beta_call(); }\n",
        )
        .unwrap();
        fs::write(root.join("noise.rs"), "fn nothing_here() {}\n").unwrap();

        let db_dir = tempfile::tempdir().unwrap();
        let db_path = db_dir.path().join("index.db");
        IndexDb::open(&db_path).unwrap().apply_changes(root).unwrap();

        let response = fts_search(&db_path, root, "beta_call", 10).unwrap();
        assert_eq!(response.strategy, "fts");
        assert_eq!(response.hits.len(), 1, "only alpha.rs contains beta_call");

        let hit = &response.hits[0];
        assert!(hit.path.ends_with("alpha.rs"), "hit path: {}", hit.path);
        assert_eq!(hit.rank, 1);
        // The scorer must land on the line that actually mentions the query, not
        // line 1 (the unrelated fn) — proves BM25-vs-substring reconciliation.
        assert_eq!(hit.location.as_deref(), Some("line 2"));
        assert!(
            hit.snippet.contains("beta_call"),
            "snippet should show the matching line: {}",
            hit.snippet
        );
        assert!(hit.score > 0.0, "score should be positive/descending");
        assert!(hit.artifact_id.starts_with("fts:"));
    }

    #[test]
    fn fts_search_orders_files_by_bm25() {
        // Two files match; BM25 ranks the denser-match file first and the
        // SearchResponse ranks/scores must follow that order.
        use tsift_index::index::IndexDb;

        let src = tempfile::tempdir().unwrap();
        let root = src.path();
        fs::write(
            root.join("dense.rs"),
            "fn widget() {}\nfn widget_two() {}\nlet w = widget();\n",
        )
        .unwrap();
        fs::write(root.join("sparse.rs"), "fn other() { /* widget */ }\n").unwrap();

        let db_dir = tempfile::tempdir().unwrap();
        let db_path = db_dir.path().join("index.db");
        IndexDb::open(&db_path).unwrap().apply_changes(root).unwrap();

        let response = fts_search(&db_path, root, "widget", 10).unwrap();
        assert_eq!(response.hits.len(), 2);
        assert!(
            response.hits[0].path.ends_with("dense.rs"),
            "dense.rs should rank first by BM25, got {}",
            response.hits[0].path
        );
        assert!(
            response.hits[0].score >= response.hits[1].score,
            "scores must be descending to preserve BM25 order"
        );
        assert_eq!(response.hits[0].rank, 1);
        assert_eq!(response.hits[1].rank, 2);
    }

    #[test]
    fn fts_search_empty_for_no_token_query_and_zero_limit() {
        use tsift_index::index::IndexDb;

        let src = tempfile::tempdir().unwrap();
        let root = src.path();
        fs::write(root.join("alpha.rs"), "fn alpha() {}\n").unwrap();
        let db_dir = tempfile::tempdir().unwrap();
        let db_path = db_dir.path().join("index.db");
        IndexDb::open(&db_path).unwrap().apply_changes(root).unwrap();

        let no_tokens = fts_search(&db_path, root, "   ", 10).unwrap();
        assert_eq!(no_tokens.strategy, "fts");
        assert!(no_tokens.hits.is_empty());

        let zero_limit = fts_search(&db_path, root, "alpha", 0).unwrap();
        assert!(zero_limit.hits.is_empty());
    }
}
