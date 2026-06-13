use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use tsift_quality::runtime_churn;
use tsift_summarize::summarize::{self, SummaryDb};

const MAX_REPEATED_LINES: usize = 8;
const MAX_LINE_FAMILIES: usize = 8;
const MAX_SIGNALS: usize = 12;

fn is_zero(value: &usize) -> bool {
    *value == 0
}

/// Sort rank for signal severities: errors before warnings before anything else.
fn signal_severity_rank(severity: &str) -> u8 {
    match severity {
        "error" => 0,
        "warning" => 1,
        _ => 2,
    }
}

/// Raw logs at or above this byte size are bulky enough that callers should
/// persist them behind an artifact handle instead of relying on inlined digest
/// groups, so the full transcript stays expandable without re-piping it.
pub const LOG_DIGEST_RAW_ARTIFACT_MIN_BYTES: usize = 4096;
const MAX_FILE_REFS: usize = 8;
const MAX_SYMBOL_REFS: usize = 16;
const MAX_STACK_GROUPS: usize = 4;
const MAX_SUMMARY_SNIPPETS: usize = 2;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum LogDigestSummaryState {
    Current,
    Stale,
    Missing,
    Unavailable,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct LogDigestSummarySnippet {
    pub symbol: String,
    pub summary: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct LogDigestSignal {
    pub severity: String,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub line: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub column: Option<usize>,
    pub occurrences: usize,
    pub summary_state: LogDigestSummaryState,
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub current_summaries: Vec<LogDigestSummarySnippet>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct LogDigestRepeatedLine {
    pub line: String,
    pub occurrences: usize,
}

/// A family of near-duplicate lines folded onto one template.
///
/// Exact-line collapsing (`repeated_lines`) only merges byte-identical lines.
/// Line families additionally fold lines that differ only by variable tokens —
/// paths, counts, timestamps, crate names, semantic versions, and hashes — so
/// noisy build/test/install progress with one changing field per line collapses
/// to a single row carrying a count, distinct-variant count, and first/last
/// sample instead of dozens of nearly-identical repeated lines.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct LogDigestLineFamily {
    pub template: String,
    pub occurrences: usize,
    pub variants: usize,
    /// How many of the folded distinct variants classified as an error. Non-zero
    /// means this family hid that many distinct failures behind the template, so
    /// consumers (and the raw-log artifact) must not treat it as benign noise.
    #[serde(skip_serializing_if = "is_zero", default)]
    pub error_variants: usize,
    pub first_sample: String,
    pub last_sample: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct LogDigestFileRef {
    pub path: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub line: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub column: Option<usize>,
    pub occurrences: usize,
    pub summary_state: LogDigestSummaryState,
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub current_summaries: Vec<LogDigestSummarySnippet>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct LogDigestSymbolRef {
    pub symbol: String,
    pub occurrences: usize,
    pub summary_state: LogDigestSummaryState,
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub current_summaries: Vec<LogDigestSummarySnippet>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct LogDigestStackGroup {
    pub frames: Vec<String>,
    pub occurrences: usize,
}

/// A handle to a persisted raw-log chunk. Lets a bounded digest reference the
/// full transcript (or a bulky group) behind a stable handle plus an expansion
/// command, instead of inlining the raw lines.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct LogDigestArtifactRef {
    pub handle: String,
    pub path: String,
    pub bytes: usize,
    pub lines: usize,
    pub expand: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct LogDigestReport {
    pub root: String,
    pub total_lines: usize,
    pub non_empty_lines: usize,
    pub signal_groups: usize,
    /// Distinct error-severity signal groups before the inline cap. Lets the
    /// token-savings gate detect when distinct failures were dropped past the
    /// cap rather than only proving the digest is small (#loggatefidelity).
    pub error_signal_groups: usize,
    pub repeated_line_groups: usize,
    pub repeated_line_occurrences: usize,
    pub line_family_groups: usize,
    pub file_ref_groups: usize,
    pub symbol_ref_groups: usize,
    pub stack_groups: usize,
    pub signals: Vec<LogDigestSignal>,
    pub repeated_lines: Vec<LogDigestRepeatedLine>,
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub line_families: Vec<LogDigestLineFamily>,
    pub file_refs: Vec<LogDigestFileRef>,
    pub symbol_refs: Vec<LogDigestSymbolRef>,
    pub stack_traces: Vec<LogDigestStackGroup>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub raw_log_artifact: Option<LogDigestArtifactRef>,
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub warnings: Vec<String>,
}

/// Whether the raw log is bulky enough that callers should persist it behind an
/// artifact handle. True when the transcript is large or when any group family
/// overflows its inline cap (so the digest is hiding folded detail worth an
/// expansion handle).
pub fn raw_log_artifact_recommended(report: &LogDigestReport, input_bytes: usize) -> bool {
    input_bytes >= LOG_DIGEST_RAW_ARTIFACT_MIN_BYTES
        || report.line_family_groups > MAX_LINE_FAMILIES
        || report.repeated_line_groups > MAX_REPEATED_LINES
        || report.signal_groups > MAX_SIGNALS
        || report.stack_groups > MAX_STACK_GROUPS
}

/// Deterministic token estimate: ~4 chars per token, rounded up. Used by the
/// fixture gate to compare raw-log tokens against digest tokens without a
/// provider tokenizer.
pub fn estimate_tokens(text: &str) -> usize {
    text.chars().count().div_ceil(4)
}

/// The bounded text an agent actually reads from a digest — signal messages,
/// line-family templates/samples, repeated lines, anchored file paths, symbol
/// refs, and warnings. The fixture gate measures token savings and checks
/// required/forbidden signals against this projection, not the raw transcript.
pub fn digest_signal_text(report: &LogDigestReport) -> String {
    let mut lines = Vec::new();
    for signal in &report.signals {
        let location = match (&signal.path, signal.line) {
            (Some(path), Some(line)) => format!(" ({path}:{line})"),
            (Some(path), None) => format!(" ({path})"),
            _ => String::new(),
        };
        lines.push(format!(
            "{}: {}{} x{}",
            signal.severity, signal.message, location, signal.occurrences
        ));
    }
    for family in &report.line_families {
        if family.error_variants > 0 {
            lines.push(format!(
                "family x{} ({} distinct errors): {}",
                family.occurrences, family.error_variants, family.template
            ));
        } else {
            lines.push(format!("family x{}: {}", family.occurrences, family.template));
        }
        lines.push(family.first_sample.clone());
        lines.push(family.last_sample.clone());
    }
    for repeated in &report.repeated_lines {
        lines.push(format!("repeat x{}: {}", repeated.occurrences, repeated.line));
    }
    for file_ref in &report.file_refs {
        match file_ref.line {
            Some(line) => lines.push(format!("{}:{}", file_ref.path, line)),
            None => lines.push(file_ref.path.clone()),
        }
    }
    for symbol in &report.symbol_refs {
        lines.push(symbol.symbol.clone());
    }
    for warning in &report.warnings {
        lines.push(format!("warning: {warning}"));
    }
    lines.join("\n")
}

/// Just the classified signal messages (with severity), used by the fixture
/// gate's false-positive guard. A benign line's tokens can still surface as a
/// file ref or symbol in `digest_signal_text` even when it is correctly NOT
/// classified as an error/warning, so the forbidden-signal check must look at
/// classification output only, not the full projection.
pub fn signal_message_text(report: &LogDigestReport) -> String {
    report
        .signals
        .iter()
        .map(|signal| format!("{}: {}", signal.severity, signal.message))
        .collect::<Vec<_>>()
        .join("\n")
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct LogDigestFixture {
    pub schema_version: u64,
    #[serde(default)]
    pub description: String,
    pub cases: Vec<LogDigestFixtureCase>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct LogDigestFixtureCase {
    pub name: String,
    /// Source ecosystem: cargo | pytest | npm | pnpm | agent-doc.
    pub ecosystem: String,
    /// Raw log lines fed through `compute`.
    pub input_lines: Vec<String>,
    /// Token-savings gate: digest must shrink raw tokens by at least this %.
    pub minimum_savings_percent: f64,
    /// False-negative guard: each substring MUST survive into the digest text.
    #[serde(default)]
    pub required_signals: Vec<String>,
    /// Optional noise guard: each substring must NOT appear in the digest text.
    #[serde(default)]
    pub forbidden_signals: Vec<String>,
    /// Fidelity guard: the maximum number of distinct error-severity signal
    /// groups allowed to be dropped past the inline cap. Defaults to 0, so a
    /// case that produces more distinct errors than the digest can inline fails
    /// unless it explicitly acknowledges the overflow — the savings-only gate
    /// could not catch this because a capped digest always reports high savings
    /// regardless of how much signal was dropped (#loggatefidelity).
    #[serde(default)]
    pub maximum_dropped_error_signals: usize,
}

#[derive(Debug, Clone, Serialize)]
pub struct LogDigestFixtureCaseReport {
    pub name: String,
    pub ecosystem: String,
    pub raw_tokens: usize,
    pub digest_tokens: usize,
    pub savings_percent: f64,
    pub minimum_savings_percent: f64,
    pub savings_ok: bool,
    pub missing_required_signals: Vec<String>,
    pub present_forbidden_signals: Vec<String>,
    pub dropped_error_signals: usize,
    pub maximum_dropped_error_signals: usize,
    pub dropped_error_signals_ok: bool,
    pub passed: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct LogDigestFixtureReport {
    pub schema_version: u64,
    pub total_cases: usize,
    pub failed_cases: usize,
    pub passed: bool,
    pub cases: Vec<LogDigestFixtureCaseReport>,
}

/// Run every fixture case through `compute`, measure token savings against the
/// digest projection, and enforce required/forbidden signal coverage. The gate
/// proves the digest both compresses (token savings) and preserves real signals
/// (no false negatives) for cargo/pytest/npm/pnpm/agent-doc logs.
pub fn evaluate_fixture(root: &Path, fixture: &LogDigestFixture) -> Result<LogDigestFixtureReport> {
    let mut cases = Vec::with_capacity(fixture.cases.len());
    let mut failed_cases = 0;
    for case in &fixture.cases {
        let mut input = case.input_lines.join("\n");
        input.push('\n');
        let report = compute(root, &input)?;
        let digest_text = digest_signal_text(&report);
        let signal_text = signal_message_text(&report);
        let raw_tokens = estimate_tokens(&input);
        let digest_tokens = estimate_tokens(&digest_text);
        let savings_percent = if raw_tokens == 0 {
            0.0
        } else {
            (1.0 - digest_tokens as f64 / raw_tokens as f64) * 100.0
        };
        let savings_ok = savings_percent >= case.minimum_savings_percent;
        // Required signals must survive somewhere in the bounded digest the
        // agent reads; forbidden signals must not be misclassified as a
        // signal (checked against classification output only).
        let missing_required_signals = case
            .required_signals
            .iter()
            .filter(|needle| !digest_text.contains(needle.as_str()))
            .cloned()
            .collect::<Vec<_>>();
        let present_forbidden_signals = case
            .forbidden_signals
            .iter()
            .filter(|needle| signal_text.contains(needle.as_str()))
            .cloned()
            .collect::<Vec<_>>();
        // Fidelity guard: how many distinct error groups the inline cap dropped.
        // Errors sort first, so shown errors == min(error_signal_groups, cap);
        // the remainder is the silently-dropped count the savings gate misses.
        let shown_error_signals = report
            .signals
            .iter()
            .filter(|signal| signal.severity == "error")
            .count();
        let dropped_error_signals = report.error_signal_groups.saturating_sub(shown_error_signals);
        let dropped_error_signals_ok =
            dropped_error_signals <= case.maximum_dropped_error_signals;
        let passed = savings_ok
            && missing_required_signals.is_empty()
            && present_forbidden_signals.is_empty()
            && dropped_error_signals_ok;
        if !passed {
            failed_cases += 1;
        }
        cases.push(LogDigestFixtureCaseReport {
            name: case.name.clone(),
            ecosystem: case.ecosystem.clone(),
            raw_tokens,
            digest_tokens,
            savings_percent,
            minimum_savings_percent: case.minimum_savings_percent,
            savings_ok,
            missing_required_signals,
            present_forbidden_signals,
            dropped_error_signals,
            maximum_dropped_error_signals: case.maximum_dropped_error_signals,
            dropped_error_signals_ok,
            passed,
        });
    }
    Ok(LogDigestFixtureReport {
        schema_version: fixture.schema_version,
        total_cases: cases.len(),
        failed_cases,
        passed: failed_cases == 0,
        cases,
    })
}

#[derive(Debug)]
struct SummaryLookup {
    relative_path: String,
    live_path: Option<PathBuf>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Anchor {
    path: String,
    line: usize,
    column: Option<usize>,
}

#[derive(Debug, Clone)]
struct SignalBuilder {
    severity: String,
    message: String,
    path: Option<String>,
    line: Option<usize>,
    column: Option<usize>,
    occurrences: usize,
}

#[derive(Debug, Clone)]
struct FileRefBuilder {
    path: String,
    line: Option<usize>,
    column: Option<usize>,
    occurrences: usize,
}

#[derive(Debug, Clone)]
struct LineFamilyBuilder {
    occurrences: usize,
    variants: BTreeSet<String>,
    /// Distinct member lines that classified as an error severity. Tracked so a
    /// family that folds many distinct *failures* (e.g. per-test `FAILED ...`
    /// lines that templatize identically) is not mistaken for benign progress
    /// noise and the hidden-error count survives past first/last sampling.
    error_variants: BTreeSet<String>,
    first_sample: String,
    last_sample: String,
}

pub fn compute(path: &Path, input: &str) -> Result<LogDigestReport> {
    let root = tsift_quality::lint::resolve_harness_root_or_canonical_path(path)?;
    let summary_db = open_summary_db_if_present(&root)?;

    let mut repeated_lines = BTreeMap::<String, usize>::new();
    let mut line_families = BTreeMap::<String, LineFamilyBuilder>::new();
    let mut signals = BTreeMap::<String, SignalBuilder>::new();
    let mut file_refs = BTreeMap::<String, FileRefBuilder>::new();
    let mut symbol_counts = BTreeMap::<String, usize>::new();
    let mut stack_blocks = Vec::<Vec<String>>::new();
    let mut current_stack = Vec::<String>::new();

    let all_lines = input.lines().collect::<Vec<_>>();
    let total_lines = all_lines.len();
    let non_empty_lines = all_lines
        .iter()
        .filter(|line| !line.trim().is_empty())
        .count();

    for raw_line in &all_lines {
        let trimmed = raw_line.trim();
        if trimmed.is_empty() {
            flush_stack_group(&mut current_stack, &mut stack_blocks);
            continue;
        }

        let normalized = normalize_line(trimmed);
        *repeated_lines.entry(normalized.clone()).or_default() += 1;
        let line_signals = classify_signals(trimmed);
        let line_is_error = line_signals.iter().any(|(severity, _)| *severity == "error");
        record_line_family(&mut line_families, &normalized, line_is_error);

        if is_stack_frame_line(trimmed) {
            current_stack.push(normalize_stack_frame(trimmed));
        } else {
            flush_stack_group(&mut current_stack, &mut stack_blocks);
        }

        let anchor = extract_anchor(trimmed);
        if let Some(anchor) = &anchor {
            record_file_ref(
                &mut file_refs,
                &root,
                &anchor.path,
                Some(anchor.line),
                anchor.column,
            )?;
        }

        let structured_fields = extract_agent_doc_log_fields(trimmed);
        let mut structured_file_paths = Vec::new();
        for path in &structured_fields.file_paths {
            if let Some(display_path) = normalize_file_ref_path(&root, path)? {
                record_display_file_ref(&mut file_refs, display_path.clone(), None, None);
                structured_file_paths.push(display_path);
            }
        }
        for symbol in structured_fields.symbol_refs {
            *symbol_counts.entry(symbol).or_default() += 1;
        }

        for (severity, message) in line_signals {
            let (path, line, column) = if let Some(anchor) = &anchor {
                (
                    Some(normalize_display_path(&root, &anchor.path)?),
                    Some(anchor.line),
                    anchor.column,
                )
            } else if let Some(path) = structured_file_paths.first() {
                (Some(path.clone()), None, None)
            } else {
                (None, None, None)
            };
            let key = format!(
                "{}|{}|{}|{}|{}",
                severity,
                path.as_deref().unwrap_or("-"),
                line.unwrap_or_default(),
                column.unwrap_or_default(),
                message
            );
            let entry = signals.entry(key).or_insert_with(|| SignalBuilder {
                severity: severity.to_string(),
                message: message.clone(),
                path: path.clone(),
                line,
                column,
                occurrences: 0,
            });
            entry.occurrences += 1;
        }

        for symbol in extract_symbol_candidates(trimmed) {
            *symbol_counts.entry(symbol).or_default() += 1;
        }
    }
    flush_stack_group(&mut current_stack, &mut stack_blocks);

    let mut repeated_line_items = repeated_lines
        .into_iter()
        .filter(|(_, count)| *count > 1)
        .map(|(line, occurrences)| LogDigestRepeatedLine { line, occurrences })
        .collect::<Vec<_>>();
    repeated_line_items.sort_by(|left, right| {
        right
            .occurrences
            .cmp(&left.occurrences)
            .then(left.line.cmp(&right.line))
    });
    let repeated_line_groups = repeated_line_items.len();
    let repeated_line_occurrences = repeated_line_items
        .iter()
        .map(|item| item.occurrences.saturating_sub(1))
        .sum();
    repeated_line_items.truncate(MAX_REPEATED_LINES);

    let mut line_family_items = line_families
        .into_iter()
        .filter(|(_, builder)| builder.variants.len() > 1)
        .map(|(template, builder)| LogDigestLineFamily {
            template,
            occurrences: builder.occurrences,
            variants: builder.variants.len(),
            error_variants: builder.error_variants.len(),
            first_sample: builder.first_sample,
            last_sample: builder.last_sample,
        })
        .collect::<Vec<_>>();
    line_family_items.sort_by(|left, right| {
        // Families that hid distinct failures rank first so the inline cap never
        // truncates away an error-bearing family in favor of benign progress noise.
        (right.error_variants > 0)
            .cmp(&(left.error_variants > 0))
            .then(right.error_variants.cmp(&left.error_variants))
            .then(right.occurrences.cmp(&left.occurrences))
            .then(right.variants.cmp(&left.variants))
            .then(left.template.cmp(&right.template))
    });
    let line_family_groups = line_family_items.len();
    line_family_items.truncate(MAX_LINE_FAMILIES);

    let mut signal_items = Vec::new();
    let mut signal_builders = signals.into_values().collect::<Vec<_>>();
    signal_builders.sort_by(|left, right| {
        // Errors first so a high-occurrence warning can never crowd a distinct
        // error out of the MAX_SIGNALS budget; then by occurrences, then message.
        signal_severity_rank(&left.severity)
            .cmp(&signal_severity_rank(&right.severity))
            .then(right.occurrences.cmp(&left.occurrences))
            .then(left.message.cmp(&right.message))
    });
    let signal_groups = signal_builders.len();
    let error_signal_groups = signal_builders
        .iter()
        .filter(|builder| builder.severity == "error")
        .count();
    for signal in signal_builders.into_iter().take(MAX_SIGNALS) {
        let (summary_state, current_summaries) = match signal.path.as_deref() {
            Some(display_path) => collect_current_file_summaries(
                summary_db.as_ref(),
                &root,
                display_path,
                signal.line,
            )?,
            None => (LogDigestSummaryState::Unavailable, Vec::new()),
        };
        signal_items.push(LogDigestSignal {
            severity: signal.severity,
            message: signal.message,
            path: signal.path,
            line: signal.line,
            column: signal.column,
            occurrences: signal.occurrences,
            summary_state,
            current_summaries,
        });
    }

    let mut file_items = Vec::new();
    let mut file_builders = file_refs.into_values().collect::<Vec<_>>();
    file_builders.sort_by(|left, right| {
        right
            .occurrences
            .cmp(&left.occurrences)
            .then(left.path.cmp(&right.path))
            .then(left.line.cmp(&right.line))
    });
    let file_ref_groups = file_builders.len();
    for file_ref in file_builders.into_iter().take(MAX_FILE_REFS) {
        let (summary_state, current_summaries) = collect_current_file_summaries(
            summary_db.as_ref(),
            &root,
            &file_ref.path,
            file_ref.line,
        )?;
        file_items.push(LogDigestFileRef {
            path: file_ref.path,
            line: file_ref.line,
            column: file_ref.column,
            occurrences: file_ref.occurrences,
            summary_state,
            current_summaries,
        });
    }

    let mut symbol_items = Vec::new();
    let mut symbol_pairs = symbol_counts.into_iter().collect::<Vec<_>>();
    symbol_pairs.sort_by(|left, right| right.1.cmp(&left.1).then(left.0.cmp(&right.0)));
    let symbol_ref_groups = symbol_pairs.len();
    for (symbol, occurrences) in symbol_pairs.into_iter().take(MAX_SYMBOL_REFS) {
        let (summary_state, current_summaries) =
            collect_current_symbol_summaries(summary_db.as_ref(), &root, &symbol)?;
        symbol_items.push(LogDigestSymbolRef {
            symbol,
            occurrences,
            summary_state,
            current_summaries,
        });
    }

    let mut stack_group_counts = BTreeMap::<Vec<String>, usize>::new();
    for frames in stack_blocks {
        *stack_group_counts.entry(frames).or_default() += 1;
    }
    let mut stack_items = stack_group_counts
        .into_iter()
        .map(|(frames, occurrences)| LogDigestStackGroup {
            frames,
            occurrences,
        })
        .collect::<Vec<_>>();
    stack_items.sort_by(|left, right| {
        right
            .occurrences
            .cmp(&left.occurrences)
            .then(left.frames.cmp(&right.frames))
    });
    let stack_groups = stack_items.len();
    stack_items.truncate(MAX_STACK_GROUPS);

    let mut warnings = Vec::new();
    if total_lines == 0 {
        warnings.push("input had no lines".to_string());
    }
    if signal_groups == 0 {
        warnings.push("no warning/error signal lines detected".to_string());
    }
    if repeated_line_groups == 0 {
        warnings.push("no repeated lines detected".to_string());
    }
    if file_ref_groups == 0 {
        warnings.push("no file anchors detected".to_string());
    }

    Ok(LogDigestReport {
        root: root.display().to_string(),
        total_lines,
        non_empty_lines,
        signal_groups,
        error_signal_groups,
        repeated_line_groups,
        repeated_line_occurrences,
        line_family_groups,
        file_ref_groups,
        symbol_ref_groups,
        stack_groups,
        signals: signal_items,
        repeated_lines: repeated_line_items,
        line_families: line_family_items,
        file_refs: file_items,
        symbol_refs: symbol_items,
        stack_traces: stack_items,
        raw_log_artifact: None,
        warnings,
    })
}

fn open_summary_db_if_present(root: &Path) -> Result<Option<SummaryDb>> {
    let db_path = root.join(".tsift/summaries.db");
    if !db_path.exists() {
        return Ok(None);
    }
    Ok(Some(SummaryDb::open_read_only_with_recovery(&db_path)?.db))
}

fn collect_current_file_summaries(
    summary_db: Option<&SummaryDb>,
    root: &Path,
    display_path: &str,
    line: Option<usize>,
) -> Result<(LogDigestSummaryState, Vec<LogDigestSummarySnippet>)> {
    let Some(summary_db) = summary_db else {
        return Ok((LogDigestSummaryState::Unavailable, Vec::new()));
    };
    let lookup = summary_lookup(root, display_path);
    let rows = summary_db.get_by_file(&lookup.relative_path)?;
    if rows.is_empty() {
        return Ok((LogDigestSummaryState::Missing, Vec::new()));
    }

    let Some(live_path) = lookup.live_path else {
        return Ok((LogDigestSummaryState::Stale, Vec::new()));
    };
    let content = match std::fs::read(&live_path) {
        Ok(content) => content,
        Err(_) => return Ok((LogDigestSummaryState::Stale, Vec::new())),
    };
    let live_hash = summarize::content_hash(&content);
    if !summary_db.is_current(&lookup.relative_path, &live_hash)? {
        return Ok((LogDigestSummaryState::Stale, Vec::new()));
    }

    let mut snippets = Vec::new();
    let line_prefix = line.map(|line_number| format!("line {line_number}"));
    for row in &rows {
        if let Some(prefix) = &line_prefix
            && row.summary.contains(prefix)
        {
            snippets.push(LogDigestSummarySnippet {
                symbol: row.symbol_name.clone(),
                summary: row.summary.trim().to_string(),
            });
        }
        if snippets.len() == MAX_SUMMARY_SNIPPETS {
            break;
        }
    }
    if snippets.is_empty() {
        for row in &rows {
            snippets.push(LogDigestSummarySnippet {
                symbol: row.symbol_name.clone(),
                summary: row.summary.trim().to_string(),
            });
            if snippets.len() == MAX_SUMMARY_SNIPPETS {
                break;
            }
        }
    }
    Ok((LogDigestSummaryState::Current, snippets))
}

fn collect_current_symbol_summaries(
    summary_db: Option<&SummaryDb>,
    root: &Path,
    symbol: &str,
) -> Result<(LogDigestSummaryState, Vec<LogDigestSummarySnippet>)> {
    let Some(summary_db) = summary_db else {
        return Ok((LogDigestSummaryState::Unavailable, Vec::new()));
    };
    let rows = summary_db.get_by_symbol(symbol)?;
    if rows.is_empty() {
        return Ok((LogDigestSummaryState::Missing, Vec::new()));
    }

    let mut current = Vec::new();
    let mut saw_stale = false;
    for row in rows {
        let live_path = root.join(&row.file_path);
        let content = match std::fs::read(&live_path) {
            Ok(content) => content,
            Err(_) => {
                saw_stale = true;
                continue;
            }
        };
        let live_hash = summarize::content_hash(&content);
        if row.content_hash != live_hash {
            saw_stale = true;
            continue;
        }
        current.push(LogDigestSummarySnippet {
            symbol: row.symbol_name,
            summary: row.summary.trim().to_string(),
        });
        if current.len() == MAX_SUMMARY_SNIPPETS {
            break;
        }
    }

    if current.is_empty() {
        return Ok((
            if saw_stale {
                LogDigestSummaryState::Stale
            } else {
                LogDigestSummaryState::Missing
            },
            Vec::new(),
        ));
    }

    Ok((LogDigestSummaryState::Current, current))
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

fn record_file_ref(
    file_refs: &mut BTreeMap<String, FileRefBuilder>,
    root: &Path,
    raw_path: &str,
    line: Option<usize>,
    column: Option<usize>,
) -> Result<()> {
    if let Some(display_path) = normalize_file_ref_path(root, raw_path)? {
        record_display_file_ref(file_refs, display_path, line, column);
    }
    Ok(())
}

fn normalize_file_ref_path(root: &Path, raw_path: &str) -> Result<Option<String>> {
    if raw_path.is_empty()
        || !looks_like_path(raw_path)
        || path_points_to_existing_directory(root, raw_path)
    {
        return Ok(None);
    }
    let display_path = normalize_display_path(root, raw_path)?;
    if display_path.is_empty() {
        return Ok(None);
    }
    Ok(Some(display_path))
}

fn path_points_to_existing_directory(root: &Path, raw_path: &str) -> bool {
    let path = Path::new(raw_path);
    let candidate = if path.is_absolute() {
        path.to_path_buf()
    } else {
        root.join(path)
    };
    candidate.is_dir()
}

fn record_display_file_ref(
    file_refs: &mut BTreeMap<String, FileRefBuilder>,
    display_path: String,
    line: Option<usize>,
    column: Option<usize>,
) {
    let key = format!(
        "{}:{}:{}",
        display_path,
        line.unwrap_or_default(),
        column.unwrap_or_default()
    );
    let entry = file_refs.entry(key).or_insert_with(|| FileRefBuilder {
        path: display_path.clone(),
        line,
        column,
        occurrences: 0,
    });
    entry.path = display_path;
    entry.line = line;
    entry.column = column;
    entry.occurrences += 1;
}

fn normalize_line(line: &str) -> String {
    line.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Accumulate a whitespace-normalized line into its near-duplicate family,
/// keyed by the variable-token-stripped template. The first sample is pinned on
/// first sight; the last sample tracks the most recent member in input order.
fn record_line_family(
    families: &mut BTreeMap<String, LineFamilyBuilder>,
    normalized: &str,
    is_error: bool,
) {
    let template = templatize_line(normalized);
    let builder = families
        .entry(template)
        .or_insert_with(|| LineFamilyBuilder {
            occurrences: 0,
            variants: BTreeSet::new(),
            error_variants: BTreeSet::new(),
            first_sample: normalized.to_string(),
            last_sample: normalized.to_string(),
        });
    builder.occurrences += 1;
    builder.variants.insert(normalized.to_string());
    if is_error {
        builder.error_variants.insert(normalized.to_string());
    }
    builder.last_sample = normalized.to_string();
}

/// Replace variable tokens (timestamps, paths, semantic versions, hashes,
/// numbers/durations/sizes, and cargo-style crate names) with stable
/// placeholders so lines that differ only by those dimensions fold together.
fn templatize_line(line: &str) -> String {
    let mut out: Vec<String> = Vec::new();
    for token in line.split_whitespace() {
        let placeholder = templatize_token(token);
        // Cargo/build progress is exactly `<Verb> <crate> v<version>`; the crate
        // name varies, so fold the bare identifier immediately before the
        // version token. Restricted to (a) an exact `<ver>` placeholder so
        // wrapped or `key=<ver>` tokens do not fold an unrelated word, and (b)
        // the canonical 3-token shape — the version is the third token after a
        // Title-case progress verb — so connectives in lines like
        // `Downgrading serde from v1.0.0` / `Updating tokio to v2.0.0` are not
        // mistaken for the crate name and folded away (#logfoldname).
        if placeholder == "<ver>" && out.len() == 2 {
            let verb_is_progress = out
                .first()
                .is_some_and(|verb| is_cargo_progress_verb(verb));
            if verb_is_progress
                && let Some(prev) = out.last_mut()
                && is_plain_name(prev)
            {
                *prev = "<name>".to_string();
            }
        }
        out.push(placeholder);
    }
    out.join(" ")
}

fn templatize_token(token: &str) -> String {
    // Bracketed epoch timestamps such as the `[1778646072]` agent-doc log prefix.
    if let Some(inner) = token.strip_prefix('[').and_then(|tok| tok.strip_suffix(']'))
        && !inner.is_empty()
        && inner.chars().all(|ch| ch.is_ascii_digit())
    {
        return "[<ts>]".to_string();
    }

    // `key=value` structured fields (agent-doc logs, `snap_len=4616`, `file=/x`):
    // keep the key, templatize only the value so lifecycle lines that vary by a
    // single field collapse together.
    if let Some((key, value)) = token.split_once('=')
        && !key.is_empty()
        && !value.is_empty()
        && key
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '_' | '-' | '.'))
    {
        let (prefix, core, suffix) = split_token_wrappers(value);
        return match classify_variable_core(core) {
            Some(placeholder) => format!("{key}={prefix}{placeholder}{suffix}"),
            None => token.to_string(),
        };
    }

    let (prefix, core, suffix) = split_token_wrappers(token);
    match classify_variable_core(core) {
        Some(placeholder) => format!("{prefix}{placeholder}{suffix}"),
        None => token.to_string(),
    }
}

/// Split surrounding punctuation so wrapped variables like `(45.2s)` templatize
/// to `(<n>)` instead of staying literal.
fn split_token_wrappers(token: &str) -> (&str, &str, &str) {
    let core = token
        .trim_start_matches(['(', '[', '{', '"', '\''])
        .trim_end_matches([')', ']', '}', '"', '\'', ',', ';', ':']);
    let prefix_len = token.len() - token.trim_start_matches(['(', '[', '{', '"', '\'']).len();
    let suffix_start = prefix_len + core.len();
    (&token[..prefix_len], core, &token[suffix_start..])
}

fn classify_variable_core(core: &str) -> Option<&'static str> {
    if core.is_empty() {
        return None;
    }
    if looks_like_path(core) {
        return Some("<path>");
    }
    if is_version_core(core) {
        return Some("<ver>");
    }
    if is_hash_core(core) {
        return Some("<hash>");
    }
    if is_numeric_core(core) {
        return Some("<n>");
    }
    None
}

/// A semantic version: optional `v` prefix plus dot-separated numeric groups.
/// Requires a `v` prefix with 2+ groups, or 3+ groups bare, so two-group
/// decimals like `45.2` stay numeric rather than versions.
fn is_version_core(core: &str) -> bool {
    let (has_v, rest) = match core.strip_prefix('v') {
        Some(rest) => (true, rest),
        None => (false, core),
    };
    let groups = rest.split('.').collect::<Vec<_>>();
    let min_groups = if has_v { 2 } else { 3 };
    groups.len() >= min_groups
        && groups
            .iter()
            .all(|group| !group.is_empty() && group.chars().all(|ch| ch.is_ascii_digit()))
}

/// A hex hash/object id: 7..=40 hex chars with at least one letter so it is not
/// mistaken for a plain number.
fn is_hash_core(core: &str) -> bool {
    let len = core.len();
    (7..=40).contains(&len)
        && core.chars().all(|ch| ch.is_ascii_hexdigit())
        && core.chars().any(|ch| ch.is_ascii_alphabetic())
}

/// A number, duration, size, or percent: an optional sign, digits with optional
/// commas and one decimal point, and an optional trailing alphabetic unit
/// (`ms`, `s`, `MB`, ...) or `%`.
fn is_numeric_core(core: &str) -> bool {
    let core = core.trim_start_matches(['+', '-']);
    let core = core.strip_suffix('%').unwrap_or(core);
    let split_at = core
        .find(|ch: char| ch.is_ascii_alphabetic())
        .unwrap_or(core.len());
    let (number, unit) = core.split_at(split_at);
    if !unit.is_empty() && !unit.chars().all(|ch| ch.is_ascii_alphabetic()) {
        return false;
    }
    let number = number.replace(',', "");
    if number.is_empty() {
        return false;
    }
    let mut seen_dot = false;
    for ch in number.chars() {
        match ch {
            '.' if !seen_dot => seen_dot = true,
            '.' => return false,
            ch if ch.is_ascii_digit() => {}
            _ => return false,
        }
    }
    number.chars().any(|ch| ch.is_ascii_digit())
}

/// A bare identifier (crate/package name) that has not already been replaced by
/// a placeholder.
fn is_plain_name(token: &str) -> bool {
    !token.starts_with('<')
        && token.chars().any(|ch| ch.is_ascii_alphabetic())
        && token
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '_' | '-'))
}

/// Cargo/build progress verbs are Title-case alphabetic words (Compiling,
/// Checking, Updating, Downgrading, Downloading, ...). Used to restrict the
/// `<name>` crate-fold to the canonical `<Verb> <crate> v<ver>` shape so
/// connectives (`from`/`to`) before a version are not folded as crate names.
fn is_cargo_progress_verb(token: &str) -> bool {
    token
        .chars()
        .next()
        .is_some_and(|first| first.is_ascii_uppercase())
        && token.chars().all(|ch| ch.is_ascii_alphabetic())
}

fn classify_signals(line: &str) -> Vec<(&'static str, String)> {
    let mut signals = Vec::new();
    if let Some(signal) = classify_generic_signal(line) {
        signals.push(signal);
    }
    signals.extend(classify_agent_doc_runtime_signals(line));
    signals.sort();
    signals.dedup();
    signals
}

fn classify_generic_signal(line: &str) -> Option<(&'static str, String)> {
    let trimmed = line.trim();
    let lower = trimmed.to_ascii_lowercase();
    if lower.starts_with("traceback")
        || lower.contains(" panicked at ")
        || lower.starts_with("panic:")
        || lower.contains("exception")
        || lower.contains("error:")
        || lower.starts_with("error ")
        || lower.starts_with("error[")
        || lower.starts_with("failed ")
        || lower.starts_with("fatal:")
        || lower.starts_with("caused by:")
        || lower.starts_with("e       ")
        || trimmed.starts_with("E   ")
        || lower.contains("test result: failed")
        || lower.contains("segmentation fault")
        || lower.contains("core dumped")
        || is_make_error_line(trimmed, &lower)
        || is_rust_assertion_failure(&lower)
        || looks_like_failed_test_summary(&lower)
        || has_npm_pnpm_error_token(trimmed)
    {
        return Some(("error", trimmed.to_string()));
    }
    if lower.starts_with("warning:")
        || lower.starts_with("warn:")
        || lower.contains(" warning ")
        || lower.contains(" warning:")
    {
        return Some(("warning", trimmed.to_string()));
    }
    None
}

/// Detect npm/yarn `ERR!` and pnpm `ERR_PNPM_*` error markers as whitespace
/// tokens rather than bare substrings, so benign lines that merely contain the
/// fragments (`stderr!`, a `/path/err_pnpm-notes` token, a URL) are not flagged.
fn has_npm_pnpm_error_token(line: &str) -> bool {
    line.split_whitespace().any(|token| {
        token.eq_ignore_ascii_case("err!")
            || token
                .get(..9)
                .is_some_and(|prefix| prefix.eq_ignore_ascii_case("err_pnpm_"))
    })
}

/// Detect test-runner failure summaries such as pytest `1 failed, 3 passed in
/// 2.1s`, `1 failed in 2.1s`, or jest `Tests: 2 failed, 5 passed`. Anchored to a
/// `<non-zero number> failed` count token so passing summaries (`0 failed`) and
/// prose (`the build failed`) are not flagged.
fn looks_like_failed_test_summary(lower: &str) -> bool {
    let tokens: Vec<&str> = lower.split_whitespace().collect();
    tokens.windows(2).any(|pair| {
        let count = pair[0];
        let word = pair[1].trim_end_matches([',', ';', '.', ':']);
        word == "failed" && count.parse::<u64>().map(|n| n > 0).unwrap_or(false)
    })
}

/// Detect GNU make error lines: `make: *** [build] Error 1`,
/// `make[1]: *** [target] Error 2`, `*** No rule to make target`. Anchored on
/// make's `*** ` sigil (line-leading or after the `<maker>: ` prefix) plus an
/// error marker, so banners that merely contain `***` are not flagged.
fn is_make_error_line(trimmed: &str, lower: &str) -> bool {
    (trimmed.starts_with("*** ") || trimmed.contains(": *** "))
        && (lower.contains("error") || lower.contains("no rule to make target"))
}

/// Detect Rust assertion panics: `assertion failed: <expr>` and the
/// `assertion `left == right` failed` form.
fn is_rust_assertion_failure(lower: &str) -> bool {
    lower.contains("assertion failed")
        || (lower.contains("assertion") && lower.contains("` failed"))
}

fn classify_agent_doc_runtime_signals(line: &str) -> Vec<(&'static str, String)> {
    let Some(event_name) = event_name_from_timestamped_line(line) else {
        return Vec::new();
    };

    let mut signals = Vec::new();
    if matches!(event_name, "claude_exit" | "codex_exit")
        && structured_field(line, "code").is_some_and(|code| code != "0")
    {
        signals.push((
            "error",
            format!(
                "agent-doc exit: {event_name} code={}",
                structured_field(line, "code").unwrap_or("?")
            ),
        ));
    }

    if event_name.contains("timeout") {
        signals.push(("warning", format!("agent-doc timeout: {event_name}")));
    }

    for family in runtime_churn::classify_restart_churn_families(event_name, line) {
        if !runtime_churn::is_restart_churn_warning_family(family) {
            continue;
        }
        signals.push(("warning", format!("agent-doc restart churn: {family}")));
    }

    if event_name == "document_cycle"
        && structured_field(line, "event") == Some("commit_already_current")
    {
        signals.push((
            "warning",
            "agent-doc closeout churn: commit_already_current".to_string(),
        ));
    }

    signals
}

fn is_stack_frame_line(line: &str) -> bool {
    let trimmed = line.trim();
    if trimmed.starts_with("at ") || parse_python_anchor(trimmed).is_some() {
        return true;
    }
    let mut chars = trimmed.chars();
    chars
        .next()
        .map(|first| first.is_ascii_digit() && trimmed.contains(": "))
        .unwrap_or(false)
}

fn normalize_stack_frame(line: &str) -> String {
    let trimmed = line.trim();
    if let Some((prefix, rest)) = trimmed.split_once(": ")
        && prefix.chars().all(|ch| ch.is_ascii_digit())
    {
        return rest.trim().to_string();
    }
    trimmed.to_string()
}

fn flush_stack_group(current: &mut Vec<String>, groups: &mut Vec<Vec<String>>) {
    if current.len() >= 2 {
        groups.push(current.clone());
    }
    current.clear();
}

fn extract_anchor(line: &str) -> Option<Anchor> {
    if let Some(anchor) = parse_python_anchor(line) {
        return Some(anchor);
    }
    for token in line.split_whitespace() {
        let cleaned = token
            .trim_matches(|c: char| matches!(c, '(' | ')' | '[' | ']' | '"' | '\'' | ','))
            .trim_end_matches(':');
        if let Some(anchor) = parse_anchor_token(cleaned) {
            return Some(anchor);
        }
        if let Some(inner) = cleaned
            .split_once('(')
            .and_then(|(_, tail)| tail.strip_suffix(')'))
            && let Some(anchor) = parse_anchor_token(inner)
        {
            return Some(anchor);
        }
    }
    None
}

#[derive(Debug, Default)]
struct AgentDocLogFields {
    file_paths: Vec<String>,
    symbol_refs: Vec<String>,
}

fn extract_agent_doc_log_fields(line: &str) -> AgentDocLogFields {
    let mut fields = AgentDocLogFields::default();

    if let Some(event) = event_name_from_timestamped_line(line) {
        fields.symbol_refs.push(format!("event:{event}"));
    }

    for token in line.split_whitespace() {
        let Some((key, value)) = token.split_once('=') else {
            continue;
        };
        let key = key.trim();
        let value = clean_structured_value(value);
        if value.is_empty() {
            continue;
        }
        match key {
            "file" | "path" if looks_like_path(value) => {
                fields.file_paths.push(value.to_string());
            }
            "event" => fields.symbol_refs.push(format!("event:{value}")),
            "pane" => fields.symbol_refs.push(format!("pane:{value}")),
            "session" => fields.symbol_refs.push(format!("session:{value}")),
            _ => {}
        }
    }

    fields.file_paths.sort();
    fields.file_paths.dedup();
    fields.symbol_refs.sort();
    fields.symbol_refs.dedup();
    fields
}

fn event_name_from_timestamped_line(line: &str) -> Option<&str> {
    let tail = line.strip_prefix('[')?;
    let (_, rest) = tail.split_once(']')?;
    let event = rest.split_whitespace().next()?;
    if event.is_empty() || event.contains('=') {
        return None;
    }
    Some(event)
}

fn clean_structured_value(value: &str) -> &str {
    value
        .trim_matches(['"', '\'', ',', ';', ')', ']', '}'])
        .trim_start_matches(['"', '\'', '(', '[', '{'])
}

fn structured_field<'a>(detail: &'a str, key: &str) -> Option<&'a str> {
    let needle = format!("{key}=");
    let start = detail.find(&needle)? + needle.len();
    let remainder = &detail[start..];
    let end = remainder
        .find(char::is_whitespace)
        .unwrap_or(remainder.len());
    let value = clean_structured_value(&remainder[..end]);
    (!value.is_empty()).then_some(value)
}

fn parse_python_anchor(line: &str) -> Option<Anchor> {
    let marker = "File \"";
    let start = line.find(marker)?;
    let tail = &line[start + marker.len()..];
    let end = tail.find('"')?;
    let path = tail[..end].trim();
    if path.is_empty() {
        return None;
    }
    let rest = &tail[end + 1..];
    let line_marker = ", line ";
    let line_start = rest.find(line_marker)?;
    let digits = rest[line_start + line_marker.len()..]
        .chars()
        .take_while(|ch| ch.is_ascii_digit())
        .collect::<String>();
    let line_number = digits.parse().ok()?;
    Some(Anchor {
        path: path.to_string(),
        line: line_number,
        column: None,
    })
}

fn parse_anchor_token(token: &str) -> Option<Anchor> {
    let mut parts = token.rsplitn(3, ':');
    let last = parts.next()?;
    let middle = parts.next()?;
    let rest = parts.next();

    if let (Ok(column), Ok(line)) = (last.parse::<usize>(), middle.parse::<usize>()) {
        let path = rest?.trim();
        if looks_like_path(path) {
            return Some(Anchor {
                path: path.to_string(),
                line,
                column: Some(column),
            });
        }
    }

    if let Ok(line) = last.parse::<usize>() {
        let path = middle.trim();
        if looks_like_path(path) {
            return Some(Anchor {
                path: path.to_string(),
                line,
                column: None,
            });
        }
    }

    None
}

fn looks_like_path(path: &str) -> bool {
    path.contains('/')
        || path.contains('\\')
        || matches!(
            Path::new(path).extension().and_then(|ext| ext.to_str()),
            Some("rs" | "py" | "ts" | "tsx" | "js" | "jsx" | "kt" | "zig" | "java" | "md")
        )
}

fn extract_symbol_candidates(line: &str) -> BTreeSet<String> {
    let mut symbols = BTreeSet::new();

    let parts = line.split('`').collect::<Vec<_>>();
    for idx in (1..parts.len()).step_by(2) {
        let candidate = parts[idx].trim();
        if is_symbol_candidate(candidate) {
            symbols.insert(candidate.to_string());
        }
    }

    if let Some((_, tail)) = line.split_once(", in ") {
        let candidate = tail
            .split_whitespace()
            .next()
            .unwrap_or_default()
            .trim_matches(|c: char| !is_symbol_char(c));
        if is_symbol_candidate(candidate) {
            symbols.insert(candidate.to_string());
        }
    }

    for token in line.split_whitespace() {
        let cleaned = token.trim_matches(|c: char| {
            matches!(
                c,
                '(' | ')' | '[' | ']' | '{' | '}' | '"' | '\'' | ',' | ';' | ':'
            )
        });
        if cleaned.is_empty() {
            continue;
        }

        if let Some((before_paren, _)) = cleaned.split_once('(')
            && is_symbol_candidate(before_paren)
        {
            symbols.insert(before_paren.to_string());
        }

        if is_symbol_candidate(cleaned) {
            symbols.insert(cleaned.to_string());
        }
    }

    symbols
}

fn is_symbol_candidate(token: &str) -> bool {
    if token.len() < 3 || token.len() > 120 {
        return false;
    }
    if token.starts_with("http://") || token.starts_with("https://") {
        return false;
    }
    if looks_like_path(token) || token.chars().all(|ch| ch.is_ascii_digit()) {
        return false;
    }
    if !token.chars().any(|ch| ch.is_ascii_alphabetic()) {
        return false;
    }
    if !token.chars().all(is_symbol_char) {
        return false;
    }
    token.contains("::")
        || token.contains('_')
        || token.contains('.')
        || (token.contains('-') && token.chars().any(|ch| ch.is_ascii_digit()))
}

fn is_symbol_char(ch: char) -> bool {
    ch.is_ascii_alphanumeric() || matches!(ch, '_' | ':' | '.' | '-')
}

#[cfg(test)]
mod tests {
    use super::*;
    use tsift_summarize::summarize::Summary;

    #[test]
    fn log_digest_collapses_repeats_and_enriches_paths_and_symbols() {
        let dir = tempfile::tempdir().unwrap();
        let file_path = dir.path().join("src/lib.rs");
        std::fs::create_dir_all(file_path.parent().unwrap()).unwrap();
        std::fs::write(&file_path, "fn run_sync() {}\n").unwrap();

        let db = SummaryDb::open(&dir.path().join(".tsift/summaries.db")).unwrap();
        let content_hash = summarize::content_hash(&std::fs::read(&file_path).unwrap());
        db.insert(&Summary {
            id: 0,
            symbol_name: "run_sync".to_string(),
            file_path: "src/lib.rs".to_string(),
            content_hash,
            summary: "run_sync coordinates the synchronous worker hand-off.".to_string(),
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
error: run_sync failed at src/lib.rs:1:1
error: run_sync failed at src/lib.rs:1:1
warning: retrying run_sync
warning: retrying run_sync
0: my_crate::run_sync
at src/lib.rs:1:1

0: my_crate::run_sync
at src/lib.rs:1:1
";

        let report = compute(dir.path(), input).unwrap();
        assert_eq!(report.signal_groups, 2);
        assert_eq!(report.repeated_line_groups, 4);
        assert!(report.repeated_line_occurrences >= 4);
        assert_eq!(report.file_refs[0].path, "src/lib.rs");
        assert_eq!(
            report.file_refs[0].summary_state,
            LogDigestSummaryState::Current
        );
        assert!(
            report
                .symbol_refs
                .iter()
                .any(|symbol| symbol.symbol == "run_sync"
                    && symbol.summary_state == LogDigestSummaryState::Current)
        );
        assert_eq!(report.stack_traces.len(), 1);
        assert_eq!(report.stack_traces[0].occurrences, 2);
    }

    #[test]
    fn templatize_line_folds_only_canonical_cargo_progress_shape() {
        // Canonical `<Verb> <crate> v<ver>` folds the crate name.
        assert_eq!(
            templatize_line("Compiling serde v1.0.193"),
            "Compiling <name> <ver>"
        );
        assert_eq!(
            templatize_line("Checking syn v2.0.80"),
            "Checking <name> <ver>"
        );

        // Connectives before a version are NOT mistaken for the crate name, so
        // `from` / `to` survive and the two lines keep distinct templates
        // (#logfoldname).
        assert_eq!(
            templatize_line("Downgrading serde from v1.0.0"),
            "Downgrading serde from <ver>"
        );
        assert_eq!(
            templatize_line("Updating tokio to v2.0.0"),
            "Updating tokio to <ver>"
        );
        assert_ne!(
            templatize_line("Downgrading serde from v1.0.0"),
            templatize_line("Updating tokio to v2.0.0")
        );

        // A lowercase, non-progress 3-token line ending in a version does not fold.
        assert_eq!(templatize_line("error in v1.0.0"), "error in <ver>");
    }

    #[test]
    fn log_digest_folds_near_duplicate_line_families() {
        let dir = tempfile::tempdir().unwrap();

        let input = "\
   Compiling serde v1.0.130
   Compiling syn v2.0.80
   Compiling tsift-cli v0.1.67
    Finished release [optimized] target(s) in 45.2s
    Finished release [optimized] target(s) in 12.8s
[1778646072] commit_staging file=tasks/a.md snap_len=4616 file_len=4664
[1778646090] commit_staging file=tasks/b.md snap_len=8192 file_len=8200
unique uncollapsed line
";

        let report = compute(dir.path(), input).unwrap();

        // Cargo crate name + version both fold to placeholders.
        let compiling = report
            .line_families
            .iter()
            .find(|family| family.template == "Compiling <name> <ver>")
            .expect("compiling family present");
        assert_eq!(compiling.occurrences, 3);
        assert_eq!(compiling.variants, 3);
        assert_eq!(compiling.first_sample, "Compiling serde v1.0.130");
        assert_eq!(compiling.last_sample, "Compiling tsift-cli v0.1.67");

        // Duration-only variance folds, keeping the literal scaffold.
        assert!(report.line_families.iter().any(|family| {
            family.template == "Finished release [optimized] target(s) in <n>"
                && family.occurrences == 2
                && family.variants == 2
        }));

        // Timestamp prefix + structured key=value lengths + path fold together.
        assert!(report.line_families.iter().any(|family| {
            family.template == "[<ts>] commit_staging file=<path> snap_len=<n> file_len=<n>"
                && family.occurrences == 2
                && family.variants == 2
        }));

        // line_family_groups counts all variant>1 families even past the cap.
        assert_eq!(report.line_family_groups, report.line_families.len());

        // A line with no near-duplicate is not reported as a family.
        assert!(
            !report
                .line_families
                .iter()
                .any(|family| family.template.contains("uncollapsed"))
        );
    }

    #[test]
    fn classifies_ecosystem_specific_error_forms() {
        let dir = tempfile::tempdir().unwrap();
        let input = "\
error[E0277]: the trait bound `T: Foo` is not satisfied
E   assert response.status_code == 200
npm ERR! code ELIFECYCLE
ERR_PNPM_RECURSIVE_RUN_FIRST_FAIL build failed
";
        let report = compute(dir.path(), input).unwrap();
        let messages = report
            .signals
            .iter()
            .map(|signal| signal.message.as_str())
            .collect::<Vec<_>>();
        assert!(
            messages.iter().any(|m| m.starts_with("error[E0277]")),
            "cargo rustc error code not classified: {messages:?}"
        );
        assert!(
            messages.iter().any(|m| m.starts_with("E   assert")),
            "pytest assertion detail not classified: {messages:?}"
        );
        assert!(
            messages.iter().any(|m| m.contains("npm ERR!")),
            "npm error not classified: {messages:?}"
        );
        assert!(
            messages.iter().any(|m| m.contains("ERR_PNPM")),
            "pnpm error not classified: {messages:?}"
        );
    }

    #[test]
    fn npm_pnpm_error_classification_is_token_anchored() {
        let dir = tempfile::tempdir().unwrap();
        let input = "\
npm ERR! code ELIFECYCLE
ERR_PNPM_RECURSIVE_RUN_FIRST_FAIL my-app@1.0.0 build
flushed stderr! buffer to disk
see the guide at https://example.com/err_pnpm-notes
downloaded err_pnpmish.tar to /tmp
";
        let report = compute(dir.path(), input).unwrap();
        let error_messages = report
            .signals
            .iter()
            .filter(|signal| signal.severity == "error")
            .map(|signal| signal.message.as_str())
            .collect::<Vec<_>>();

        // Real npm/pnpm markers are still classified.
        assert!(error_messages.iter().any(|m| m.contains("npm ERR!")));
        assert!(error_messages.iter().any(|m| m.contains("ERR_PNPM_RECURSIVE")));

        // Benign lines that merely contain the fragments are NOT flagged.
        assert!(
            !error_messages.iter().any(|m| m.contains("stderr!")),
            "stderr! must not be classified as an npm error: {error_messages:?}"
        );
        assert!(
            !error_messages.iter().any(|m| m.contains("err_pnpm-notes")),
            "a mid-token err_pnpm fragment must not be classified: {error_messages:?}"
        );
        assert!(
            !error_messages.iter().any(|m| m.contains("err_pnpmish")),
            "err_pnpmish is not a pnpm error code: {error_messages:?}"
        );
    }

    #[test]
    fn classifies_test_runner_and_build_failure_summaries() {
        let dir = tempfile::tempdir().unwrap();
        let input = "\
test result: FAILED. 2 passed; 1 failed; 0 ignored
1 failed, 3 passed in 2.1s
Tests: 2 failed, 5 passed
make: *** [build] Error 1
Segmentation fault (core dumped)
assertion `left == right` failed
assertion failed: response.ok
";
        let report = compute(dir.path(), input).unwrap();
        let errors = report
            .signals
            .iter()
            .filter(|signal| signal.severity == "error")
            .map(|signal| signal.message.as_str())
            .collect::<Vec<_>>();

        for needle in [
            "test result: FAILED",
            "1 failed, 3 passed",
            "Tests: 2 failed",
            "*** [build] Error 1",
            "Segmentation fault",
            "assertion `left == right` failed",
            "assertion failed: response.ok",
        ] {
            assert!(
                errors.iter().any(|m| m.contains(needle)),
                "failure line not classified ({needle:?}): {errors:?}"
            );
        }
    }

    #[test]
    fn failure_summary_classification_is_token_anchored() {
        let dir = tempfile::tempdir().unwrap();
        // Every line here is benign and must NOT be classified as an error.
        let input = "\
test result: ok. 49 passed; 0 failed; 0 ignored
0 failed, 12 passed in 0.4s
the build failed to find a cached artifact, retrying
*** Welcome to the build banner ***
this assertion about latency held under load
";
        let report = compute(dir.path(), input).unwrap();
        let errors = report
            .signals
            .iter()
            .filter(|signal| signal.severity == "error")
            .map(|signal| signal.message.as_str())
            .collect::<Vec<_>>();
        assert!(
            errors.is_empty(),
            "benign lines must not be classified as errors: {errors:?}"
        );
    }

    #[test]
    fn line_family_surfaces_distinct_error_count_when_failures_fold() {
        let dir = tempfile::tempdir().unwrap();
        let mut input = String::new();
        for idx in 0..20 {
            input.push_str(&format!("FAILED tests/test_{idx}.py::t - assert {idx} == 200\n"));
        }
        let report = compute(dir.path(), &input).unwrap();

        // All 20 distinct failures templatize identically and fold into one family...
        let family = report
            .line_families
            .iter()
            .find(|family| family.template.contains("FAILED"))
            .expect("expected a folded FAILED family");
        assert_eq!(
            family.variants, 20,
            "all 20 distinct lines fold to one template"
        );
        // ...but the distinct-error count survives past first/last sampling, so the
        // family is not mistaken for benign progress noise.
        assert_eq!(
            family.error_variants, 20,
            "folded family must surface the hidden distinct-error count"
        );

        // The inline signal list is still bounded, but the total is countable and
        // the raw artifact is recommended so the over-cap failures are not lost.
        assert_eq!(report.signal_groups, 20);
        assert!(report.signals.len() <= MAX_SIGNALS);
        assert!(raw_log_artifact_recommended(&report, input.len()));

        // The text projection names the hidden distinct-error count.
        assert!(
            digest_signal_text(&report).contains("distinct errors"),
            "digest text should surface the distinct-error count"
        );
    }

    #[test]
    fn error_signals_are_not_crowded_out_by_high_occurrence_warnings() {
        let dir = tempfile::tempdir().unwrap();
        let mut input = String::new();
        // 15 repetitions of 13 distinct warnings (high occurrence) ...
        for _ in 0..15 {
            for idx in 0..13 {
                input.push_str(&format!("warning: deprecated api {idx} in use\n"));
            }
        }
        // ... plus a single, low-occurrence real error.
        input.push_str("error[E0599]: no method named `foo` found\n");
        let report = compute(dir.path(), &input).unwrap();

        let kept_error = report
            .signals
            .iter()
            .any(|signal| signal.severity == "error" && signal.message.contains("E0599"));
        assert!(
            kept_error,
            "a single error must survive the cap ahead of high-occurrence warnings: {:?}",
            report
                .signals
                .iter()
                .map(|s| (&s.severity, &s.message))
                .collect::<Vec<_>>()
        );
    }

    #[test]
    fn evaluate_fixture_enforces_savings_and_false_negative_guards() {
        let dir = tempfile::tempdir().unwrap();

        let mut input_lines = vec![
            "error[E0277]: the trait bound is not satisfied".to_string(),
        ];
        // Pad with bulky near-duplicate progress so token savings are real.
        for idx in 0..60 {
            input_lines.push(format!("   Compiling crate_{idx} v0.1.{idx}"));
        }

        // Passing case: real savings + the required error survives the digest.
        let pass = LogDigestFixture {
            schema_version: 1,
            description: String::new(),
            cases: vec![LogDigestFixtureCase {
                name: "cargo-build".to_string(),
                ecosystem: "cargo".to_string(),
                input_lines: input_lines.clone(),
                minimum_savings_percent: 50.0,
                required_signals: vec!["error[E0277]".to_string()],
                forbidden_signals: vec![],
                maximum_dropped_error_signals: 0,
            }],
        };
        let report = evaluate_fixture(dir.path(), &pass).unwrap();
        assert!(report.passed, "expected pass: {:?}", report.cases);
        assert!(report.cases[0].savings_percent >= 50.0);
        assert!(report.cases[0].digest_tokens < report.cases[0].raw_tokens);

        // Failing case: a required signal that the digest never produces.
        let fail = LogDigestFixture {
            schema_version: 1,
            description: String::new(),
            cases: vec![LogDigestFixtureCase {
                name: "missing-signal".to_string(),
                ecosystem: "cargo".to_string(),
                input_lines,
                minimum_savings_percent: 50.0,
                required_signals: vec!["this signal is never emitted".to_string()],
                forbidden_signals: vec![],
                maximum_dropped_error_signals: 0,
            }],
        };
        let report = evaluate_fixture(dir.path(), &fail).unwrap();
        assert!(!report.passed);
        assert_eq!(report.failed_cases, 1);
        assert_eq!(
            report.cases[0].missing_required_signals,
            vec!["this signal is never emitted".to_string()]
        );
    }

    #[test]
    fn fixture_gate_flags_distinct_errors_dropped_past_the_cap() {
        let dir = tempfile::tempdir().unwrap();
        let input_lines: Vec<String> = (0..20)
            .map(|idx| format!("error: distinct failure {idx} in module m{idx}"))
            .collect();

        // Default maximum_dropped_error_signals = 0 → the gate must FAIL: 20
        // distinct errors cannot all inline under the 12-signal cap, even though
        // the capped digest reports high token savings (#loggatefidelity).
        let strict = LogDigestFixture {
            schema_version: 1,
            description: String::new(),
            cases: vec![LogDigestFixtureCase {
                name: "many-distinct-errors".to_string(),
                ecosystem: "cargo".to_string(),
                input_lines: input_lines.clone(),
                minimum_savings_percent: -1000.0,
                required_signals: vec![],
                forbidden_signals: vec![],
                maximum_dropped_error_signals: 0,
            }],
        };
        let report = evaluate_fixture(dir.path(), &strict).unwrap();
        assert!(
            !report.passed,
            "gate must flag silently dropped distinct errors: {:?}",
            report.cases
        );
        assert!(report.cases[0].dropped_error_signals > 0);
        assert!(!report.cases[0].dropped_error_signals_ok);

        // Acknowledging the overflow lets the case pass (the raw-log artifact
        // carries the dropped errors).
        let lenient = LogDigestFixture {
            schema_version: 1,
            description: String::new(),
            cases: vec![LogDigestFixtureCase {
                name: "many-distinct-errors".to_string(),
                ecosystem: "cargo".to_string(),
                input_lines,
                minimum_savings_percent: -1000.0,
                required_signals: vec![],
                forbidden_signals: vec![],
                maximum_dropped_error_signals: 100,
            }],
        };
        let report = evaluate_fixture(dir.path(), &lenient).unwrap();
        assert!(report.cases[0].dropped_error_signals_ok);
    }

    #[test]
    fn fixture_forbidden_signal_checks_classification_not_full_projection() {
        let dir = tempfile::tempdir().unwrap();

        // A benign line whose symbol-like token (`err_pnpmish-fixture`) leaks
        // into the digest's symbol refs even though it is NOT classified as an
        // error. The forbidden guard must look at signal classification only,
        // so this case passes; a full-projection check would wrongly fail it.
        let fixture = LogDigestFixture {
            schema_version: 1,
            description: String::new(),
            cases: vec![LogDigestFixtureCase {
                name: "precision".to_string(),
                ecosystem: "npm".to_string(),
                input_lines: vec![
                    "npm http fetch GET 200 https://registry.npmjs.org/react 142ms".to_string(),
                    "npm http fetch GET 200 https://registry.npmjs.org/axios 155ms".to_string(),
                    "downloaded err_pnpmish-fixture sample into the cache".to_string(),
                    "npm ERR! code ELIFECYCLE".to_string(),
                ],
                // Negative threshold isolates the forbidden/required logic from
                // savings on this deliberately tiny input.
                minimum_savings_percent: -1000.0,
                required_signals: vec!["npm ERR!".to_string()],
                forbidden_signals: vec!["err_pnpmish".to_string()],
                maximum_dropped_error_signals: 0,
            }],
        };
        let report = evaluate_fixture(dir.path(), &fixture).unwrap();
        assert!(report.passed, "precision case should pass: {:?}", report.cases);
        assert!(report.cases[0].present_forbidden_signals.is_empty());

        // Sanity: the benign symbol token IS in the full projection (so a naive
        // full-projection forbidden check would have failed) but NOT in the
        // classified signal text.
        let computed = compute(
            dir.path(),
            "downloaded err_pnpmish-fixture sample into the cache\nnpm ERR! code ELIFECYCLE\n",
        )
        .unwrap();
        assert!(digest_signal_text(&computed).contains("err_pnpmish"));
        assert!(!signal_message_text(&computed).contains("err_pnpmish"));
        assert!(signal_message_text(&computed).contains("npm ERR!"));
    }

    #[test]
    fn raw_log_artifact_recommended_for_bulky_or_overflowing_logs() {
        let dir = tempfile::tempdir().unwrap();

        // Small clean log: no artifact recommended.
        let small = compute(dir.path(), "Compiling serde v1.0.130\n").unwrap();
        assert!(!raw_log_artifact_recommended(&small, "Compiling serde v1.0.130\n".len()));

        // Same small report but a bulky byte count crosses the size threshold.
        assert!(raw_log_artifact_recommended(
            &small,
            LOG_DIGEST_RAW_ARTIFACT_MIN_BYTES
        ));
        assert!(!raw_log_artifact_recommended(
            &small,
            LOG_DIGEST_RAW_ARTIFACT_MIN_BYTES - 1
        ));

        // Many distinct signal lines overflow the inline signal cap and trigger
        // the handle recommendation even for a byte-small transcript.
        let mut many_signals = String::new();
        for idx in 0..(MAX_SIGNALS + 4) {
            many_signals.push_str(&format!("error: distinct failure number {idx}\n"));
        }
        let overflow = compute(dir.path(), &many_signals).unwrap();
        assert!(overflow.signal_groups > MAX_SIGNALS);
        assert!(raw_log_artifact_recommended(&overflow, many_signals.len()));
    }

    #[test]
    fn log_digest_parses_python_file_anchors() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("tests")).unwrap();
        std::fs::write(
            dir.path().join("tests/test_sample.py"),
            "def test_fail():\n    pass\n",
        )
        .unwrap();

        let input = "\
Traceback (most recent call last):
  File \"tests/test_sample.py\", line 12, in test_fail
    helper()
RuntimeError: boom
";

        let report = compute(dir.path(), input).unwrap();
        assert!(
            report.file_refs.iter().any(
                |file_ref| file_ref.path == "tests/test_sample.py" && file_ref.line == Some(12)
            )
        );
        assert!(
            report
                .symbol_refs
                .iter()
                .any(|symbol| symbol.symbol == "test_fail")
        );
    }

    #[test]
    fn log_digest_parses_agent_doc_structured_fields() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("tasks/software")).unwrap();
        std::fs::write(dir.path().join("tasks/software/tsift.md"), "# tsift\n").unwrap();
        std::fs::write(dir.path().join("tasks/software/absolute.md"), "# abs\n").unwrap();

        let input = format!(
            "\
[1778646072] route_dispatch_start_proven file=tasks/software/tsift.md pane=%31 harness=codex proof=consumed timeout_secs=10
[1778646073] cwd_resolved path={} source=project_root
[1778646078] document_cycle phase=committed cycle=cycle-1778644920810 event=commit_success session=tsift-v0.1 pane=%31
[1778646078] commit_staging file={} snap_len=4616 file_len=4664
",
            dir.path().display(),
            dir.path().join("tasks/software/absolute.md").display()
        );

        let report = compute(dir.path(), &input).unwrap();
        assert_eq!(report.file_ref_groups, 2);
        assert!(
            report
                .file_refs
                .iter()
                .any(|file_ref| file_ref.path == "tasks/software/tsift.md"
                    && file_ref.line.is_none())
        );
        assert!(
            report
                .file_refs
                .iter()
                .any(|file_ref| file_ref.path.ends_with("tasks/software/absolute.md"))
        );
        assert!(
            !report
                .file_refs
                .iter()
                .any(|file_ref| file_ref.path.is_empty())
        );
        for expected in [
            "event:route_dispatch_start_proven",
            "event:cwd_resolved",
            "event:commit_success",
            "pane:%31",
            "session:tsift-v0.1",
        ] {
            assert!(
                report
                    .symbol_refs
                    .iter()
                    .any(|symbol| symbol.symbol == expected),
                "missing structured symbol {expected}"
            );
        }
        assert!(
            !report
                .warnings
                .iter()
                .any(|warning| warning == "no file anchors detected")
        );
    }

    #[test]
    fn log_digest_classifies_agent_doc_runtime_churn_as_signals() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("tasks/software")).unwrap();
        std::fs::write(dir.path().join("tasks/software/tsift.md"), "# tsift\n").unwrap();

        let input = "\
[1776528398] claude_start mode=fresh_restart restart_count=1 file=tasks/software/tsift.md
[1776528446] auto_trigger_timeout harness=codex reason=no_prompt_after_30s
[1776528450] ctrl_d_restart_fresh restart_count=2 file=tasks/software/tsift.md
[1776528451] user_quit_after_ctrl_d
[1776528452] supervisor_exit reason=user_quit_after_eof restart_count=0
[1776528532] claude_exit code=1 restart_count=0
[1777603403] document_cycle phase=committed cycle=cycle-1 event=commit_already_current
[1777603404] document_cycle phase=committed cycle=cycle-2 event=commit_already_current
";

        let report = compute(dir.path(), input).unwrap();
        assert_eq!(report.signal_groups, 6);
        assert!(
            !report
                .warnings
                .iter()
                .any(|warning| warning == "no warning/error signal lines detected")
        );
        assert!(report.signals.iter().any(|signal| {
            signal.severity == "error"
                && signal.message == "agent-doc exit: claude_exit code=1"
                && signal.occurrences == 1
        }));
        assert!(report.signals.iter().any(|signal| {
            signal.message == "agent-doc timeout: auto_trigger_timeout" && signal.occurrences == 1
        }));
        assert!(report.signals.iter().any(|signal| {
            signal.message == "agent-doc restart churn: fresh_restart" && signal.occurrences == 2
        }));
        assert!(report.signals.iter().any(|signal| {
            signal.message == "agent-doc restart churn: auto_trigger_timeout"
                && signal.occurrences == 1
        }));
        assert!(report.signals.iter().any(|signal| {
            signal.message == "agent-doc restart churn: ctrl_d_restart_loop"
                && signal.occurrences == 1
        }));
        assert!(
            !report
                .signals
                .iter()
                .any(|signal| signal.message == "agent-doc restart churn: quit_after_eof")
        );
        assert!(report.signals.iter().any(|signal| {
            signal.message == "agent-doc closeout churn: commit_already_current"
                && signal.occurrences == 2
        }));
    }
}
