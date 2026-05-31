use std::cmp::Ordering;
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
        let mut candidates = Vec::new();
        let mut indexed_artifacts = 0usize;
        let mut skipped_artifacts = 0usize;
        for path in candidate_files(&input.root)? {
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
        assert_eq!(response.indexed_artifacts, 2);
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
}
