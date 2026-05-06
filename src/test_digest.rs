use crate::summarize::{self, SummaryDb};
use anyhow::{Result, bail};
use serde::Serialize;
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum TestRunner {
    Cargo,
    Pytest,
    Unknown,
}

impl TestRunner {
    pub fn parse(raw: &str) -> Result<Self> {
        match raw.trim().to_ascii_lowercase().as_str() {
            "cargo" | "rust" => Ok(Self::Cargo),
            "pytest" | "py" | "python" => Ok(Self::Pytest),
            "auto" => Ok(Self::Unknown),
            other => bail!("unsupported runner `{other}`; expected cargo, pytest, or auto"),
        }
    }

    pub fn detect(input: &str) -> Self {
        if input.contains("test result: FAILED.")
            || input.contains("failures:")
            || input.contains("thread '")
        {
            return Self::Cargo;
        }
        if input.contains("short test summary info")
            || input.contains("= FAILURES =")
            || input.contains("FAILED ")
        {
            return Self::Pytest;
        }
        Self::Unknown
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Cargo => "cargo",
            Self::Pytest => "pytest",
            Self::Unknown => "unknown",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum TestDigestSummaryState {
    Current,
    Stale,
    Missing,
    Unavailable,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct TestDigestSummarySnippet {
    pub symbol: String,
    pub summary: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct TestDigestFailure {
    pub tests: Vec<String>,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub line: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub column: Option<usize>,
    pub occurrences: usize,
    pub summary_state: TestDigestSummaryState,
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub current_summaries: Vec<TestDigestSummarySnippet>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct TestDigestCounts {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub passed: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub failed: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub skipped: Option<usize>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct TestDigestReport {
    pub root: String,
    pub runner: String,
    pub failures: usize,
    pub grouped_failures: usize,
    pub counts: TestDigestCounts,
    pub failure_groups: Vec<TestDigestFailure>,
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub warnings: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct RawFailure {
    test_name: String,
    message: String,
    path: Option<String>,
    line: Option<usize>,
    column: Option<usize>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct RawCounts {
    passed: Option<usize>,
    failed: Option<usize>,
    skipped: Option<usize>,
}

#[derive(Debug, Clone, Default)]
struct RawDigest {
    failures: Vec<RawFailure>,
    counts: RawCounts,
    warnings: Vec<String>,
}

#[derive(Debug)]
struct SummaryLookup {
    relative_path: String,
    live_path: Option<PathBuf>,
}

#[derive(Debug, Default)]
struct FailureGroupBuilder {
    tests: BTreeSet<String>,
    message: String,
    path: Option<String>,
    line: Option<usize>,
    column: Option<usize>,
    occurrences: usize,
}

pub fn compute(path: &Path, input: &str, runner: Option<&str>) -> Result<TestDigestReport> {
    let root = crate::lint::resolve_harness_root_or_canonical_path(path)?;
    let selected_runner = match runner {
        Some(raw) => {
            let parsed = TestRunner::parse(raw)?;
            if parsed == TestRunner::Unknown {
                TestRunner::detect(input)
            } else {
                parsed
            }
        }
        None => TestRunner::detect(input),
    };

    let parsed = match selected_runner {
        TestRunner::Cargo => parse_cargo_output(input),
        TestRunner::Pytest => parse_pytest_output(input),
        TestRunner::Unknown => parse_generic_output(input),
    };
    let summary_db = open_summary_db_if_present(&root)?;

    let mut grouped = BTreeMap::<String, FailureGroupBuilder>::new();
    for failure in parsed.failures {
        let display_path = failure
            .path
            .as_deref()
            .map(|raw| normalize_display_path(&root, raw))
            .transpose()?;
        let key = format!(
            "{}:{}:{}:{}",
            display_path.as_deref().unwrap_or("-"),
            failure.line.unwrap_or_default(),
            failure.column.unwrap_or_default(),
            failure.message
        );
        let entry = grouped.entry(key).or_default();
        entry.tests.insert(failure.test_name);
        entry.message = failure.message;
        entry.path = display_path;
        entry.line = failure.line;
        entry.column = failure.column;
        entry.occurrences += 1;
    }

    let mut failure_groups = Vec::new();
    for (_, grouped_failure) in grouped {
        let (summary_state, current_summaries) = match grouped_failure.path.as_deref() {
            Some(display_path) => collect_current_summaries(
                summary_db.as_ref(),
                &root,
                display_path,
                grouped_failure.line,
            )?,
            None => (TestDigestSummaryState::Unavailable, Vec::new()),
        };
        failure_groups.push(TestDigestFailure {
            tests: grouped_failure.tests.into_iter().collect(),
            message: grouped_failure.message,
            path: grouped_failure.path,
            line: grouped_failure.line,
            column: grouped_failure.column,
            occurrences: grouped_failure.occurrences,
            summary_state,
            current_summaries,
        });
    }

    failure_groups.sort_by(|left, right| {
        left.path
            .cmp(&right.path)
            .then(left.line.cmp(&right.line))
            .then(left.message.cmp(&right.message))
    });

    Ok(TestDigestReport {
        root: root.display().to_string(),
        runner: selected_runner.as_str().to_string(),
        failures: failure_groups
            .iter()
            .map(|failure| failure.occurrences)
            .sum(),
        grouped_failures: failure_groups.len(),
        counts: TestDigestCounts {
            passed: parsed.counts.passed,
            failed: parsed.counts.failed.or_else(|| {
                (!failure_groups.is_empty()).then_some(
                    failure_groups
                        .iter()
                        .map(|failure| failure.occurrences)
                        .sum(),
                )
            }),
            skipped: parsed.counts.skipped,
        },
        failure_groups,
        warnings: parsed.warnings,
    })
}

fn open_summary_db_if_present(root: &Path) -> Result<Option<SummaryDb>> {
    let db_path = root.join(".tsift/summaries.db");
    if !db_path.exists() {
        return Ok(None);
    }
    Ok(Some(SummaryDb::open_read_only_with_recovery(&db_path)?.db))
}

fn collect_current_summaries(
    summary_db: Option<&SummaryDb>,
    root: &Path,
    display_path: &str,
    line: Option<usize>,
) -> Result<(TestDigestSummaryState, Vec<TestDigestSummarySnippet>)> {
    let Some(summary_db) = summary_db else {
        return Ok((TestDigestSummaryState::Unavailable, Vec::new()));
    };
    let lookup = summary_lookup(root, display_path);
    let rows = summary_db.get_by_file(&lookup.relative_path)?;
    if rows.is_empty() {
        return Ok((TestDigestSummaryState::Missing, Vec::new()));
    }

    let Some(live_path) = lookup.live_path else {
        return Ok((TestDigestSummaryState::Stale, Vec::new()));
    };
    let content = match std::fs::read(&live_path) {
        Ok(content) => content,
        Err(_) => return Ok((TestDigestSummaryState::Stale, Vec::new())),
    };
    let live_hash = summarize::content_hash(&content);
    if !summary_db.is_current(&lookup.relative_path, &live_hash)? {
        return Ok((TestDigestSummaryState::Stale, Vec::new()));
    }

    let mut snippets = Vec::new();
    let line_prefix = line.map(|line_number| format!("line {line_number}"));
    for row in &rows {
        if let Some(prefix) = &line_prefix
            && row.summary.contains(prefix)
        {
            snippets.push(TestDigestSummarySnippet {
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
            snippets.push(TestDigestSummarySnippet {
                symbol: row.symbol_name.clone(),
                summary: row.summary.trim().to_string(),
            });
            if snippets.len() == 2 {
                break;
            }
        }
    }

    Ok((TestDigestSummaryState::Current, snippets))
}

fn summary_lookup(root: &Path, display_path: &str) -> SummaryLookup {
    let path = Path::new(display_path);
    if path.is_absolute() {
        let live_path = path.to_path_buf();
        let relative_path = live_path
            .strip_prefix(root)
            .ok()
            .map(summarize::normalize_summary_file_key)
            .unwrap_or_else(|| display_path.to_string());
        return SummaryLookup {
            relative_path,
            live_path: Some(live_path),
        };
    }

    let normalized = summarize::normalize_summary_file_key(path);
    let live_path = root.join(&normalized);
    SummaryLookup {
        relative_path: normalized,
        live_path: Some(live_path),
    }
}

fn normalize_display_path(root: &Path, raw: &str) -> Result<String> {
    let path = Path::new(raw);
    if path.is_absolute() {
        return Ok(path
            .strip_prefix(root)
            .map(summarize::normalize_summary_file_key)
            .unwrap_or_else(|_| path.display().to_string()));
    }
    Ok(summarize::normalize_summary_file_key(path))
}

fn parse_cargo_output(input: &str) -> RawDigest {
    let mut digest = RawDigest::default();
    let lines = input.lines().collect::<Vec<_>>();
    let mut idx = 0;

    while idx < lines.len() {
        let line = lines[idx];
        if let Some(test_name) = parse_cargo_failure_header(line) {
            idx += 1;
            let mut block = Vec::new();
            while idx < lines.len() {
                let current = lines[idx];
                if parse_cargo_failure_header(current).is_some()
                    || current == "failures:"
                    || current.starts_with("test result:")
                {
                    break;
                }
                block.push(current);
                idx += 1;
            }
            digest
                .failures
                .push(parse_failure_block(test_name.to_string(), &block));
            continue;
        }
        if line.starts_with("test result:") {
            digest.counts = parse_cargo_counts(line);
        }
        idx += 1;
    }

    if digest.failures.is_empty() {
        digest
            .warnings
            .push("no cargo failure blocks found in input".to_string());
    }
    digest
}

fn parse_pytest_output(input: &str) -> RawDigest {
    let mut digest = RawDigest::default();
    let lines = input.lines().collect::<Vec<_>>();
    let mut idx = 0;
    let mut saw_failure_block = false;

    while idx < lines.len() {
        let line = lines[idx];
        if let Some(test_name) = parse_pytest_failure_header(line) {
            saw_failure_block = true;
            idx += 1;
            let mut block = Vec::new();
            while idx < lines.len() {
                let current = lines[idx];
                if parse_pytest_failure_header(current).is_some()
                    || current.starts_with("short test summary info")
                    || current.starts_with("====")
                {
                    break;
                }
                block.push(current);
                idx += 1;
            }
            digest
                .failures
                .push(parse_failure_block(test_name.to_string(), &block));
            continue;
        }
        if line.starts_with("FAILED ") && !saw_failure_block {
            digest.failures.push(parse_pytest_summary_failure(line));
        }
        if line.starts_with('=') && line.contains(" failed") {
            digest.counts = parse_pytest_counts(line);
        }
        idx += 1;
    }

    if digest.failures.is_empty() {
        digest
            .warnings
            .push("no pytest failure blocks found in input".to_string());
    }
    dedupe_failures(digest)
}

fn parse_generic_output(input: &str) -> RawDigest {
    let mut digest = RawDigest::default();
    let mut seen = BTreeSet::new();
    for line in input.lines() {
        if !line.contains("FAILED") && !line.contains("error:") {
            continue;
        }
        let anchor = extract_anchor(line);
        let key = format!("{}::{:?}", line.trim(), anchor);
        if !seen.insert(key) {
            continue;
        }
        digest.failures.push(RawFailure {
            test_name: "failure".to_string(),
            message: line.trim().to_string(),
            path: anchor.as_ref().map(|anchor| anchor.path.clone()),
            line: anchor.as_ref().map(|anchor| anchor.line),
            column: anchor.and_then(|anchor| anchor.column),
        });
    }
    if digest.failures.is_empty() {
        digest
            .warnings
            .push("runner auto-detection failed; no failure signatures found".to_string());
    }
    digest
}

fn parse_failure_block(test_name: String, block: &[&str]) -> RawFailure {
    let mut message = String::new();
    let mut path = None;
    let mut line = None;
    let mut column = None;

    for entry in block {
        let trimmed = entry.trim();
        if trimmed.is_empty()
            || trimmed.starts_with("note:")
            || trimmed.starts_with("stack backtrace")
        {
            continue;
        }
        if path.is_none()
            && let Some(anchor) = extract_anchor(trimmed)
        {
            let anchor_text = anchor.render();
            path = Some(anchor.path);
            line = Some(anchor.line);
            column = anchor.column;
            if let Some(idx) = trimmed.find(&anchor_text) {
                let tail = trimmed[idx + anchor_text.len()..]
                    .trim_start_matches(':')
                    .trim();
                if !tail.is_empty() {
                    message = tail.to_string();
                }
            }
            continue;
        }
        if message.is_empty() && !trimmed.starts_with("thread '") && !trimmed.starts_with('>') {
            message = trimmed
                .trim_start_matches("E ")
                .trim_start_matches("assert ")
                .trim()
                .to_string();
        }
    }

    if message.is_empty() {
        message = "test failed".to_string();
    }

    RawFailure {
        test_name,
        message,
        path,
        line,
        column,
    }
}

fn parse_pytest_summary_failure(line: &str) -> RawFailure {
    let trimmed = line.trim_start_matches("FAILED ").trim();
    let (test_name, message) = trimmed
        .split_once(" - ")
        .map(|(name, msg)| (name.trim().to_string(), msg.trim().to_string()))
        .unwrap_or_else(|| (trimmed.to_string(), "test failed".to_string()));
    let path = test_name
        .split("::")
        .next()
        .map(|raw| raw.trim().to_string())
        .filter(|value| !value.is_empty());
    RawFailure {
        test_name,
        message,
        path,
        line: None,
        column: None,
    }
}

fn dedupe_failures(mut digest: RawDigest) -> RawDigest {
    let mut unique = BTreeSet::new();
    digest.failures.retain(|failure| {
        unique.insert(format!(
            "{}|{}|{:?}|{:?}|{:?}",
            failure.test_name, failure.message, failure.path, failure.line, failure.column
        ))
    });
    digest
}

fn parse_cargo_failure_header(line: &str) -> Option<&str> {
    if !(line.starts_with("---- ") && line.ends_with(" ----")) {
        return None;
    }
    let inner = line.trim_start_matches("---- ").trim_end_matches(" ----");
    Some(
        inner
            .trim_end_matches(" stdout")
            .trim_end_matches(" stderr")
            .trim(),
    )
}

fn parse_pytest_failure_header(line: &str) -> Option<&str> {
    let trimmed = line.trim();
    if !trimmed.starts_with('_') {
        return None;
    }
    let name = trimmed.trim_matches('_').trim();
    if name.is_empty() {
        return None;
    }
    Some(name)
}

fn parse_cargo_counts(line: &str) -> RawCounts {
    let mut counts = RawCounts::default();
    for part in line.split(';') {
        let trimmed = part.trim().trim_end_matches('.');
        if let Some(value) = leading_number(trimmed) {
            if trimmed.contains(" passed") {
                counts.passed = Some(value);
            } else if trimmed.contains(" failed") {
                counts.failed = Some(value);
            } else if trimmed.contains(" ignored") {
                counts.skipped = Some(value);
            }
        }
    }
    counts
}

fn parse_pytest_counts(line: &str) -> RawCounts {
    let mut counts = RawCounts::default();
    let trimmed = line.trim_matches('=').trim();
    for part in trimmed.split(',') {
        let entry = part.trim();
        if let Some(value) = leading_number(entry) {
            if entry.contains(" passed") {
                counts.passed = Some(value);
            } else if entry.contains(" failed") {
                counts.failed = Some(value);
            } else if entry.contains(" skipped") {
                counts.skipped = Some(value);
            }
        }
    }
    counts
}

fn leading_number(input: &str) -> Option<usize> {
    input
        .split_whitespace()
        .find_map(|token| token.trim_end_matches('.').parse().ok())
}

#[derive(Debug, Clone)]
struct Anchor {
    path: String,
    line: usize,
    column: Option<usize>,
}

impl Anchor {
    fn render(&self) -> String {
        match self.column {
            Some(column) => format!("{}:{}:{}", self.path, self.line, column),
            None => format!("{}:{}", self.path, self.line),
        }
    }
}

fn extract_anchor(line: &str) -> Option<Anchor> {
    for token in line.split_whitespace() {
        let cleaned = token
            .trim_matches(|c: char| matches!(c, '(' | ')' | '[' | ']' | '"' | '\'' | ','))
            .trim_end_matches(':');
        if let Some(anchor) = parse_anchor_token(cleaned) {
            return Some(anchor);
        }
    }
    None
}

fn parse_anchor_token(token: &str) -> Option<Anchor> {
    let mut parts = token.rsplitn(3, ':');
    let last = parts.next()?;
    let middle = parts.next()?;
    let rest = parts.next();

    if let (Ok(column), Ok(line)) = (last.parse::<usize>(), middle.parse::<usize>()) {
        let path = rest?.trim();
        if !path.is_empty() {
            return Some(Anchor {
                path: path.to_string(),
                line,
                column: Some(column),
            });
        }
    }

    if let Ok(line) = last.parse::<usize>() {
        let path = middle.trim();
        if !path.is_empty() {
            return Some(Anchor {
                path: path.to_string(),
                line,
                column: None,
            });
        }
    }

    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::summarize::Summary;

    #[test]
    fn cargo_digest_groups_duplicate_failures_and_reads_counts() {
        let dir = tempfile::tempdir().unwrap();
        let file_path = dir.path().join("src/lib.rs");
        std::fs::create_dir_all(file_path.parent().unwrap()).unwrap();
        std::fs::write(&file_path, "fn helper() {}\n").unwrap();

        let db = SummaryDb::open(&dir.path().join(".tsift/summaries.db")).unwrap();
        let content_hash = summarize::content_hash(&std::fs::read(&file_path).unwrap());
        db.insert(&Summary {
            id: 0,
            symbol_name: "helper".to_string(),
            file_path: "src/lib.rs".to_string(),
            content_hash,
            summary: "helper keeps the shared test fixture stable.".to_string(),
            entities: None,
            relationships: None,
            concept_labels: None,
            extracted_at: "0".to_string(),
            model: "test".to_string(),
            tokens_input: Some(1),
            tokens_output: Some(1),
        })
        .unwrap();

        let input = "\
running 3 tests
---- tests::alpha stdout ----
thread 'tests::alpha' panicked at src/lib.rs:7:9:
assertion `left == right` failed

---- tests::beta stdout ----
thread 'tests::beta' panicked at src/lib.rs:7:9:
assertion `left == right` failed

failures:
    tests::alpha
    tests::beta

test result: FAILED. 1 passed; 2 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.00s
";

        let report = compute(dir.path(), input, Some("cargo")).unwrap();
        assert_eq!(report.runner, "cargo");
        assert_eq!(report.failures, 2);
        assert_eq!(report.grouped_failures, 1);
        assert_eq!(report.counts.passed, Some(1));
        assert_eq!(report.counts.failed, Some(2));
        assert_eq!(report.failure_groups[0].path.as_deref(), Some("src/lib.rs"));
        assert_eq!(report.failure_groups[0].line, Some(7));
        assert_eq!(report.failure_groups[0].occurrences, 2);
        assert_eq!(
            report.failure_groups[0].tests,
            vec!["tests::alpha".to_string(), "tests::beta".to_string()]
        );
        assert_eq!(
            report.failure_groups[0].summary_state,
            TestDigestSummaryState::Current
        );
        assert_eq!(report.failure_groups[0].current_summaries.len(), 1);
    }

    #[test]
    fn pytest_digest_extracts_anchor_and_counts() {
        let dir = tempfile::tempdir().unwrap();
        let file_path = dir.path().join("tests/test_sample.py");
        std::fs::create_dir_all(file_path.parent().unwrap()).unwrap();
        std::fs::write(&file_path, "def test_fail():\n    assert False\n").unwrap();

        let input = "\
____________________________ test_fail ____________________________

    def test_fail():
>       assert False
E       assert False

tests/test_sample.py:12: AssertionError
=========================== short test summary info ============================
FAILED tests/test_sample.py::test_fail - AssertionError: assert False
========================= 1 failed, 2 passed in 0.11s =========================
";

        let report = compute(dir.path(), input, Some("pytest")).unwrap();
        assert_eq!(report.runner, "pytest");
        assert_eq!(report.failures, 1);
        assert_eq!(report.grouped_failures, 1);
        assert_eq!(report.counts.passed, Some(2));
        assert_eq!(report.counts.failed, Some(1));
        assert_eq!(
            report.failure_groups[0].path.as_deref(),
            Some("tests/test_sample.py")
        );
        assert_eq!(report.failure_groups[0].line, Some(12));
        assert_eq!(report.failure_groups[0].message, "AssertionError");
    }
}
