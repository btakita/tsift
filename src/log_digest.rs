use crate::runtime_churn;
use crate::summarize::{self, SummaryDb};
use anyhow::Result;
use serde::Serialize;
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

const MAX_REPEATED_LINES: usize = 8;
const MAX_SIGNALS: usize = 12;
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

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct LogDigestReport {
    pub root: String,
    pub total_lines: usize,
    pub non_empty_lines: usize,
    pub signal_groups: usize,
    pub repeated_line_groups: usize,
    pub repeated_line_occurrences: usize,
    pub file_ref_groups: usize,
    pub symbol_ref_groups: usize,
    pub stack_groups: usize,
    pub signals: Vec<LogDigestSignal>,
    pub repeated_lines: Vec<LogDigestRepeatedLine>,
    pub file_refs: Vec<LogDigestFileRef>,
    pub symbol_refs: Vec<LogDigestSymbolRef>,
    pub stack_traces: Vec<LogDigestStackGroup>,
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub warnings: Vec<String>,
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

pub fn compute(path: &Path, input: &str) -> Result<LogDigestReport> {
    let root = crate::lint::resolve_harness_root_or_canonical_path(path)?;
    let summary_db = open_summary_db_if_present(&root)?;

    let mut repeated_lines = BTreeMap::<String, usize>::new();
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

        *repeated_lines.entry(normalize_line(trimmed)).or_default() += 1;

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

        for (severity, message) in classify_signals(trimmed) {
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

    let mut signal_items = Vec::new();
    let mut signal_builders = signals.into_values().collect::<Vec<_>>();
    signal_builders.sort_by(|left, right| {
        right
            .occurrences
            .cmp(&left.occurrences)
            .then(left.severity.cmp(&right.severity))
            .then(left.message.cmp(&right.message))
    });
    let signal_groups = signal_builders.len();
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
        repeated_line_groups,
        repeated_line_occurrences,
        file_ref_groups,
        symbol_ref_groups,
        stack_groups,
        signals: signal_items,
        repeated_lines: repeated_line_items,
        file_refs: file_items,
        symbol_refs: symbol_items,
        stack_traces: stack_items,
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
        || lower.starts_with("failed ")
        || lower.starts_with("fatal:")
        || lower.starts_with("caused by:")
        || lower.starts_with("e       ")
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
    use crate::summarize::Summary;

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
        assert!(report.signals.iter().any(|signal| {
            signal.message == "agent-doc closeout churn: commit_already_current"
                && signal.occurrences == 2
        }));
    }
}
