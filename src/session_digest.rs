use anyhow::{Context, Result, bail};
use serde::Serialize;
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

const MAX_PROMPT_TARGETS: usize = 8;
const MAX_COMMANDS: usize = 12;
const MAX_FILES: usize = 12;
const MAX_SYMBOLS: usize = 12;
const MAX_FAILURES: usize = 12;
const MAX_CLOSEOUT: usize = 10;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionDigestSource {
    Markdown,
    Jsonl,
}

impl SessionDigestSource {
    pub fn parse(raw: &str) -> Result<Self> {
        match raw.trim().to_ascii_lowercase().as_str() {
            "markdown" | "md" => Ok(Self::Markdown),
            "jsonl" | "json-lines" | "claude-jsonl" => Ok(Self::Jsonl),
            other => bail!("unsupported session source `{other}`; expected markdown or jsonl"),
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Markdown => "markdown",
            Self::Jsonl => "jsonl",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SessionDigestCommand {
    pub command: String,
    pub occurrences: usize,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SessionDigestFileRef {
    pub path: String,
    pub occurrences: usize,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SessionDigestSymbolRef {
    pub symbol: String,
    pub occurrences: usize,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SessionDigestFailure {
    pub kind: String,
    pub message: String,
    pub occurrences: usize,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SessionDigestCloseout {
    pub kind: String,
    pub detail: String,
    pub occurrences: usize,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SessionDigestReport {
    pub root: String,
    pub source: String,
    pub total_lines: usize,
    pub transcript_items: usize,
    pub prompt_target_count: usize,
    pub command_groups: usize,
    pub file_groups: usize,
    pub symbol_groups: usize,
    pub failure_groups: usize,
    pub closeout_groups: usize,
    pub prompt_targets: Vec<String>,
    pub commands: Vec<SessionDigestCommand>,
    pub touched_files: Vec<SessionDigestFileRef>,
    pub touched_symbols: Vec<SessionDigestSymbolRef>,
    pub failures: Vec<SessionDigestFailure>,
    pub closeout: Vec<SessionDigestCloseout>,
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub warnings: Vec<String>,
}

#[derive(Debug, Default)]
struct DigestState {
    prompt_targets: Vec<String>,
    commands: BTreeMap<String, usize>,
    files: BTreeMap<String, usize>,
    symbols: BTreeMap<String, usize>,
    failures: BTreeMap<(String, String), usize>,
    closeout: BTreeMap<(String, String), usize>,
    warnings: Vec<String>,
    transcript_items: usize,
}

#[derive(Debug, Clone)]
enum TranscriptBlock {
    Text { role: Option<String>, text: String },
    ToolUse { name: String, input: Value },
}

pub fn compute(path: &Path, input: &str, source_hint: Option<&str>) -> Result<SessionDigestReport> {
    if input.trim().is_empty() {
        bail!("no session input provided; pass --input <file> or pipe transcript on stdin");
    }

    let root = crate::lint::resolve_project_root_or_canonical_path(path)?;
    let source = resolve_source(input, source_hint)?;
    let total_lines = input.lines().count();
    let mut state = DigestState::default();

    match source {
        SessionDigestSource::Markdown => ingest_markdown(&root, input, &mut state)?,
        SessionDigestSource::Jsonl => ingest_jsonl(&root, input, &mut state)?,
    }

    let prompt_target_count = state.prompt_targets.len();

    let mut commands = state
        .commands
        .into_iter()
        .map(|(command, occurrences)| SessionDigestCommand {
            command,
            occurrences,
        })
        .collect::<Vec<_>>();
    commands.sort_by(|left, right| {
        right
            .occurrences
            .cmp(&left.occurrences)
            .then(left.command.cmp(&right.command))
    });
    let command_groups = commands.len();
    commands.truncate(MAX_COMMANDS);

    let mut touched_files = state
        .files
        .into_iter()
        .map(|(path, occurrences)| SessionDigestFileRef { path, occurrences })
        .collect::<Vec<_>>();
    touched_files.sort_by(|left, right| {
        right
            .occurrences
            .cmp(&left.occurrences)
            .then(left.path.cmp(&right.path))
    });
    let file_groups = touched_files.len();
    touched_files.truncate(MAX_FILES);

    let mut touched_symbols = state
        .symbols
        .into_iter()
        .map(|(symbol, occurrences)| SessionDigestSymbolRef {
            symbol,
            occurrences,
        })
        .collect::<Vec<_>>();
    touched_symbols.sort_by(|left, right| {
        right
            .occurrences
            .cmp(&left.occurrences)
            .then(left.symbol.cmp(&right.symbol))
    });
    let symbol_groups = touched_symbols.len();
    touched_symbols.truncate(MAX_SYMBOLS);

    let mut failures = state
        .failures
        .into_iter()
        .map(|((kind, message), occurrences)| SessionDigestFailure {
            kind,
            message,
            occurrences,
        })
        .collect::<Vec<_>>();
    failures.sort_by(|left, right| {
        right
            .occurrences
            .cmp(&left.occurrences)
            .then(left.kind.cmp(&right.kind))
            .then(left.message.cmp(&right.message))
    });
    let failure_groups = failures.len();
    failures.truncate(MAX_FAILURES);

    let mut closeout = state
        .closeout
        .into_iter()
        .map(|((kind, detail), occurrences)| SessionDigestCloseout {
            kind,
            detail,
            occurrences,
        })
        .collect::<Vec<_>>();
    closeout.sort_by(|left, right| {
        right
            .occurrences
            .cmp(&left.occurrences)
            .then(left.kind.cmp(&right.kind))
            .then(left.detail.cmp(&right.detail))
    });
    let closeout_groups = closeout.len();
    closeout.truncate(MAX_CLOSEOUT);

    Ok(SessionDigestReport {
        root: root.display().to_string(),
        source: source.as_str().to_string(),
        total_lines,
        transcript_items: state.transcript_items,
        prompt_target_count,
        command_groups,
        file_groups,
        symbol_groups,
        failure_groups,
        closeout_groups,
        prompt_targets: state.prompt_targets,
        commands,
        touched_files,
        touched_symbols,
        failures,
        closeout,
        warnings: state.warnings,
    })
}

fn resolve_source(input: &str, source_hint: Option<&str>) -> Result<SessionDigestSource> {
    match source_hint {
        Some(raw) => SessionDigestSource::parse(raw),
        None => {
            let non_empty = input
                .lines()
                .map(str::trim)
                .filter(|line| !line.is_empty())
                .collect::<Vec<_>>();
            if !non_empty.is_empty()
                && non_empty.iter().all(|line| line.starts_with('{'))
                && non_empty
                    .iter()
                    .all(|line| serde_json::from_str::<Value>(line).is_ok())
            {
                Ok(SessionDigestSource::Jsonl)
            } else {
                Ok(SessionDigestSource::Markdown)
            }
        }
    }
}

fn ingest_markdown(root: &Path, input: &str, state: &mut DigestState) -> Result<()> {
    for line in input.lines() {
        state.transcript_items += 1;
        ingest_text_line(root, line, false, state)?;
    }
    Ok(())
}

fn ingest_jsonl(root: &Path, input: &str, state: &mut DigestState) -> Result<()> {
    for (index, raw_line) in input.lines().enumerate() {
        let trimmed = raw_line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let value = serde_json::from_str::<Value>(trimmed)
            .with_context(|| format!("parsing transcript jsonl line {}", index + 1))?;
        let mut blocks = Vec::new();
        collect_transcript_blocks(&value, &mut blocks);
        if blocks.is_empty() {
            state.warnings.push(format!(
                "jsonl line {} did not contain message content or tool_use blocks",
                index + 1
            ));
            continue;
        }
        for block in blocks {
            state.transcript_items += 1;
            match block {
                TranscriptBlock::Text { role, text } => {
                    let user_bias = role
                        .as_deref()
                        .is_some_and(|value| value.eq_ignore_ascii_case("user"));
                    for line in text.lines() {
                        ingest_text_line(root, line, user_bias, state)?;
                    }
                }
                TranscriptBlock::ToolUse { name, input } => {
                    ingest_tool_use(root, &name, &input, state)?;
                }
            }
        }
    }
    Ok(())
}

fn collect_transcript_blocks(value: &Value, out: &mut Vec<TranscriptBlock>) {
    if let Some(message) = value.get("message") {
        collect_message_blocks(message, out);
        return;
    }
    collect_message_blocks(value, out);
}

fn collect_message_blocks(value: &Value, out: &mut Vec<TranscriptBlock>) {
    let role = value
        .get("role")
        .and_then(Value::as_str)
        .map(|value| value.to_string());
    if let Some(content) = value.get("content") {
        match content {
            Value::String(text) => out.push(TranscriptBlock::Text {
                role,
                text: text.to_string(),
            }),
            Value::Array(items) => {
                for item in items {
                    collect_content_block(role.clone(), item, out);
                }
            }
            _ => {}
        }
    } else if let Some(text) = value.get("text").and_then(Value::as_str) {
        out.push(TranscriptBlock::Text {
            role,
            text: text.to_string(),
        });
    }
}

fn collect_content_block(role: Option<String>, value: &Value, out: &mut Vec<TranscriptBlock>) {
    let block_type = value.get("type").and_then(Value::as_str);
    match block_type {
        Some("text") => {
            if let Some(text) = value.get("text").and_then(Value::as_str) {
                out.push(TranscriptBlock::Text {
                    role,
                    text: text.to_string(),
                });
            }
        }
        Some("tool_use") => {
            let name = value
                .get("name")
                .and_then(Value::as_str)
                .unwrap_or("tool_use")
                .to_string();
            let input = value.get("input").cloned().unwrap_or(Value::Null);
            out.push(TranscriptBlock::ToolUse { name, input });
        }
        Some("tool_result") => match value.get("content") {
            Some(Value::String(text)) => out.push(TranscriptBlock::Text {
                role,
                text: text.to_string(),
            }),
            Some(Value::Array(items)) => {
                for item in items {
                    collect_content_block(role.clone(), item, out);
                }
            }
            _ => {}
        },
        _ => {
            if let Some(text) = value.get("text").and_then(Value::as_str) {
                out.push(TranscriptBlock::Text {
                    role,
                    text: text.to_string(),
                });
            }
        }
    }
}

fn ingest_tool_use(root: &Path, name: &str, input: &Value, state: &mut DigestState) -> Result<()> {
    if let Some(command) = extract_tool_command(name, input) {
        *state.commands.entry(command.clone()).or_default() += 1;
        for path in extract_file_refs(&command, root) {
            *state.files.entry(path).or_default() += 1;
        }
        for symbol in extract_symbol_refs(&command) {
            *state.symbols.entry(symbol).or_default() += 1;
        }
    }

    if let Some(text) = extract_tool_text(input) {
        for line in text.lines() {
            ingest_text_line(root, line, false, state)?;
        }
    }
    Ok(())
}

fn extract_tool_command(name: &str, input: &Value) -> Option<String> {
    if !matches!(
        name.to_ascii_lowercase().as_str(),
        "bash" | "exec_command" | "shell" | "terminal" | "sh"
    ) {
        return None;
    }

    match input {
        Value::Object(map) => {
            for key in ["command", "cmd", "shell_command"] {
                if let Some(raw) = map.get(key).and_then(Value::as_str) {
                    let normalized = normalize_whitespace(raw);
                    if looks_like_command(&normalized) {
                        return Some(normalized);
                    }
                }
            }
            None
        }
        Value::String(raw) => {
            let normalized = normalize_whitespace(raw);
            looks_like_command(&normalized).then_some(normalized)
        }
        _ => None,
    }
}

fn extract_tool_text(input: &Value) -> Option<String> {
    match input {
        Value::Object(map) => {
            for key in ["text", "output", "stderr", "stdout", "content", "message"] {
                if let Some(raw) = map.get(key).and_then(Value::as_str) {
                    return Some(raw.to_string());
                }
            }
            None
        }
        Value::String(raw) => Some(raw.to_string()),
        _ => None,
    }
}

fn ingest_text_line(
    root: &Path,
    raw_line: &str,
    user_bias: bool,
    state: &mut DigestState,
) -> Result<()> {
    let trimmed = raw_line.trim();
    if trimmed.is_empty() {
        return Ok(());
    }

    let prompt_candidate = trimmed
        .strip_prefix("❯ ")
        .or_else(|| trimmed.strip_prefix("> "))
        .unwrap_or(trimmed)
        .trim();
    if looks_like_prompt_target(prompt_candidate, user_bias || prompt_candidate != trimmed) {
        push_prompt_target(prompt_candidate, &mut state.prompt_targets);
    }

    for command in extract_commands(trimmed) {
        *state.commands.entry(command.clone()).or_default() += 1;
        for path in extract_file_refs(&command, root) {
            *state.files.entry(path).or_default() += 1;
        }
        for symbol in extract_symbol_refs(&command) {
            *state.symbols.entry(symbol).or_default() += 1;
        }
    }

    for path in extract_file_refs(trimmed, root) {
        *state.files.entry(path).or_default() += 1;
    }
    for symbol in extract_symbol_refs(trimmed) {
        *state.symbols.entry(symbol).or_default() += 1;
    }

    if let Some((kind, message)) = classify_failure(trimmed) {
        *state.failures.entry((kind, message)).or_default() += 1;
    }
    for (kind, detail) in detect_closeout(trimmed) {
        *state.closeout.entry((kind, detail)).or_default() += 1;
    }

    Ok(())
}

fn push_prompt_target(prompt: &str, targets: &mut Vec<String>) {
    let normalized = normalize_whitespace(prompt);
    if normalized.is_empty() || targets.iter().any(|existing| existing == &normalized) {
        return;
    }
    if targets.len() < MAX_PROMPT_TARGETS {
        targets.push(normalized);
    }
}

fn looks_like_prompt_target(text: &str, user_bias: bool) -> bool {
    let trimmed = text.trim();
    if trimmed.is_empty()
        || trimmed.starts_with("###")
        || trimmed.starts_with("<!--")
        || trimmed.starts_with("- [")
        || trimmed == "###"
    {
        return false;
    }

    if trimmed.starts_with("do ")
        || trimmed.starts_with('#')
        || trimmed.starts_with('/')
        || trimmed.ends_with('?')
    {
        return true;
    }

    if user_bias
        && (trimmed.contains("commit + push")
            || trimmed.contains("run tests")
            || trimmed.contains("build + install")
            || trimmed.contains("#spec-test"))
    {
        return true;
    }

    false
}

fn extract_commands(text: &str) -> Vec<String> {
    let mut commands = BTreeSet::new();
    for span in extract_backtick_spans(text) {
        let normalized = normalize_whitespace(&span);
        if looks_like_command(&normalized) {
            commands.insert(normalized);
        }
    }

    let stripped = strip_common_prefixes(text.trim());
    let normalized = normalize_whitespace(stripped);
    if looks_like_command(&normalized) {
        commands.insert(normalized);
    }

    commands.into_iter().collect()
}

fn extract_backtick_spans(text: &str) -> Vec<String> {
    let mut spans = Vec::new();
    let mut start = None;
    for (index, ch) in text.char_indices() {
        if ch != '`' {
            continue;
        }
        match start {
            Some(span_start) => {
                if index > span_start + 1 {
                    spans.push(text[span_start + 1..index].to_string());
                }
                start = None;
            }
            None => start = Some(index),
        }
    }
    spans
}

fn strip_common_prefixes(text: &str) -> &str {
    text.strip_prefix("❯ ")
        .or_else(|| text.strip_prefix("- "))
        .or_else(|| text.strip_prefix("* "))
        .or_else(|| text.strip_prefix("> "))
        .unwrap_or(text)
        .trim()
}

fn looks_like_command(text: &str) -> bool {
    if text.is_empty()
        || text.contains('\n')
        || text.contains("://")
        || text.starts_with('/')
        || text.starts_with("###")
    {
        return false;
    }

    let head = text.split_whitespace().next().unwrap_or_default();
    matches!(
        head,
        "agent-doc"
            | "cargo"
            | "git"
            | "make"
            | "pytest"
            | "python"
            | "uv"
            | "tsift"
            | "npm"
            | "pnpm"
            | "yarn"
            | "bash"
            | "zsh"
            | "rg"
            | "grep"
            | "./scripts/run_benchmark.sh"
    ) || head.starts_with("./")
}

fn extract_file_refs(text: &str, root: &Path) -> Vec<String> {
    let mut paths = BTreeSet::new();
    for raw in text.split_whitespace() {
        if let Some(path) = normalize_file_token(raw, root) {
            paths.insert(path);
        }
    }
    paths.into_iter().collect()
}

fn normalize_file_token(raw: &str, root: &Path) -> Option<String> {
    let trimmed = raw.trim_matches(|ch: char| {
        matches!(
            ch,
            '`' | '"' | '\'' | ',' | ';' | '(' | ')' | '[' | ']' | '<' | '>' | '{' | '}' | '*'
        )
    });
    if trimmed.is_empty() || trimmed == "." || trimmed == "-" || trimmed.contains("://") {
        return None;
    }

    let without_line = strip_line_suffix(trimmed);
    let candidate = without_line.trim_end_matches('/');
    if candidate.is_empty() || candidate == "." {
        return None;
    }
    if !looks_like_file_path(candidate) {
        return None;
    }

    Some(normalize_display_path(root, candidate))
}

fn strip_line_suffix(token: &str) -> &str {
    let bytes = token.as_bytes();
    let mut cut = token.len();
    let mut colon_segments = 0;
    while let Some(colon_index) = token[..cut].rfind(':') {
        let suffix = &token[colon_index + 1..cut];
        if suffix.is_empty() || !suffix.chars().all(|ch| ch.is_ascii_digit()) {
            break;
        }
        colon_segments += 1;
        cut = colon_index;
        if colon_segments == 2 {
            break;
        }
        if colon_index == 0 || bytes[colon_index - 1] == b'/' {
            continue;
        }
    }
    &token[..cut]
}

fn looks_like_file_path(token: &str) -> bool {
    if token.starts_with("--") || token.starts_with('#') {
        return false;
    }

    if token.contains('/') {
        return true;
    }

    let lower = token.to_ascii_lowercase();
    matches!(
        lower.as_str(),
        "cargo.toml"
            | "cargo.lock"
            | "readme.md"
            | "agents.md"
            | "claude.md"
            | "spec.md"
            | "versions.md"
    ) || [
        ".rs", ".md", ".toml", ".json", ".jsonl", ".yaml", ".yml", ".txt", ".py", ".ts", ".tsx",
        ".js", ".jsx", ".sh", ".zsh", ".sql", ".db",
    ]
    .iter()
    .any(|suffix| lower.ends_with(suffix))
}

fn normalize_display_path(root: &Path, raw: &str) -> String {
    let path = Path::new(raw);
    if path.is_absolute() {
        if let Ok(relative) = path.strip_prefix(root) {
            return normalize_path_string(relative);
        }
        return normalize_path_string(path);
    }
    normalize_path_string(path)
}

fn normalize_path_string(path: &Path) -> String {
    path.components()
        .fold(PathBuf::new(), |mut acc, component| {
            acc.push(component.as_os_str());
            acc
        })
        .display()
        .to_string()
        .replace('\\', "/")
        .trim_start_matches("./")
        .to_string()
}

fn extract_symbol_refs(text: &str) -> Vec<String> {
    let mut symbols = BTreeSet::new();
    for span in extract_backtick_spans(text) {
        let candidate = span.trim().trim_end_matches("()");
        if looks_like_symbol(candidate) {
            symbols.insert(candidate.to_string());
        }
    }

    for raw in text.split(|ch: char| !matches!(ch, 'a'..='z' | 'A'..='Z' | '0'..='9' | '_' | ':')) {
        let candidate = raw.trim().trim_end_matches("()");
        if looks_like_symbol(candidate) {
            symbols.insert(candidate.to_string());
        }
    }

    symbols.into_iter().collect()
}

fn looks_like_symbol(candidate: &str) -> bool {
    if candidate.len() < 3
        || candidate.contains('/')
        || candidate.contains('.')
        || candidate.starts_with('#')
        || matches!(
            candidate,
            "Error" | "FAILED" | "cargo" | "pytest" | "agent" | "commit" | "push"
        )
    {
        return false;
    }

    let lower = candidate.to_ascii_lowercase();
    if matches!(
        lower.as_str(),
        "none"
            | "error"
            | "failed"
            | "warning"
            | "commit"
            | "pushed"
            | "status"
            | "stdout"
            | "stderr"
    ) {
        return false;
    }

    candidate.contains('_') || candidate.contains("::")
}

fn classify_failure(text: &str) -> Option<(String, String)> {
    let normalized = normalize_whitespace(strip_common_prefixes(text));
    let lower = normalized.to_ascii_lowercase();
    let kind = if lower.contains("timed out") {
        "timeout"
    } else if lower.starts_with("error") || lower.contains(" error:") || lower.contains("error:") {
        "error"
    } else if lower.contains("panicked") || lower.contains("panic") {
        "panic"
    } else if lower.contains("not found")
        || lower.contains(" is missing")
        || lower.contains(" missing ")
    {
        "missing"
    } else if lower.contains("failed") || lower.contains("failure") {
        "failure"
    } else {
        return None;
    };
    Some((kind.to_string(), truncate_detail(&normalized, 220)))
}

fn detect_closeout(text: &str) -> Vec<(String, String)> {
    let mut out = Vec::new();
    let normalized = normalize_whitespace(strip_common_prefixes(text));
    let lower = normalized.to_ascii_lowercase();

    if lower.contains("verification passed") || lower.starts_with("verification in ") {
        out.push((
            "verification".to_string(),
            truncate_detail(&normalized, 220),
        ));
    }
    if lower.contains("cargo build")
        || lower.contains("make check")
        || lower.contains("cargo test")
        || lower.contains("pytest")
    {
        out.push((
            "verification".to_string(),
            truncate_detail(&normalized, 220),
        ));
    }
    if lower.contains("cargo install") || lower.contains("installed") {
        out.push(("install".to_string(), truncate_detail(&normalized, 220)));
    }
    if lower.contains("committed and pushed") {
        out.push(("push".to_string(), truncate_detail(&normalized, 220)));
    } else if lower.contains("committed") {
        out.push(("commit".to_string(), truncate_detail(&normalized, 220)));
    }
    if lower.contains("tsift --version") || lower.contains("tsift v0.") {
        out.push(("version".to_string(), truncate_detail(&normalized, 220)));
    }
    if lower.contains("agent-doc finalize") || lower.contains("session-check") {
        out.push(("closeout".to_string(), truncate_detail(&normalized, 220)));
    }

    dedupe_pairs(out)
}

fn dedupe_pairs(items: Vec<(String, String)>) -> Vec<(String, String)> {
    let mut seen = BTreeSet::new();
    let mut deduped = Vec::new();
    for item in items {
        if seen.insert(item.clone()) {
            deduped.push(item);
        }
    }
    deduped
}

fn normalize_whitespace(raw: &str) -> String {
    raw.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn truncate_detail(text: &str, max_chars: usize) -> String {
    if text.chars().count() <= max_chars {
        return text.to_string();
    }
    let mut truncated = String::new();
    for ch in text.chars().take(max_chars.saturating_sub(1)) {
        truncated.push(ch);
    }
    truncated.push('…');
    truncated
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn markdown_digest_extracts_prompt_commands_failures_and_closeout() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("src")).unwrap();
        std::fs::write(dir.path().join("src/lib.rs"), "fn run_sync() {}\n").unwrap();

        let input = "\
❯ Why was this symbol search attempted?
Symbol `run_sync` not found in index.
Error: tsift search timed out after 30s at src/lib.rs:7:9
Verification in `src/tsift`: `cargo test`, `make check`, `cargo build --release`, `cargo install --path . --force`
Committed and pushed in `src/tsift` as `1af09d3` (`feat: add metric run digest`).
do [#sessiondigest]. spec-test-build-install-commit-push
";

        let report = compute(dir.path(), input, None).unwrap();
        assert_eq!(report.source, "markdown");
        assert!(
            report
                .prompt_targets
                .iter()
                .any(|target| target.contains("Why was this symbol search attempted?"))
        );
        assert!(
            report
                .prompt_targets
                .iter()
                .any(|target| target.contains("[#sessiondigest]"))
        );
        assert!(
            report
                .commands
                .iter()
                .any(|command| command.command == "cargo test")
        );
        assert!(
            report
                .touched_files
                .iter()
                .any(|path| path.path == "src/lib.rs")
        );
        assert!(
            report
                .touched_symbols
                .iter()
                .any(|symbol| symbol.symbol == "run_sync")
        );
        assert!(
            report
                .failures
                .iter()
                .any(|failure| failure.kind == "timeout")
        );
        assert!(
            report
                .closeout
                .iter()
                .any(|entry| entry.kind == "verification")
        );
        assert!(report.closeout.iter().any(|entry| entry.kind == "push"));
    }

    #[test]
    fn jsonl_digest_extracts_user_prompt_and_shell_command() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("src")).unwrap();
        std::fs::write(dir.path().join("src/lib.rs"), "fn run_sync() {}\n").unwrap();

        let input = concat!(
            r#"{"message":{"role":"user","content":"do [#sessiondigest]. spec-test-build-install-commit-push"}}"#,
            "\n",
            r#"{"message":{"role":"assistant","content":[{"type":"tool_use","name":"Bash","input":{"command":"cargo test --release --manifest-path Cargo.toml"}},{"type":"text","text":"Symbol `run_sync` not found in index.\nCommitted and pushed in `src/tsift` as `1af09d3`."}]}}"#,
            "\n"
        );

        let report = compute(dir.path(), input, None).unwrap();
        assert_eq!(report.source, "jsonl");
        assert!(
            report
                .prompt_targets
                .iter()
                .any(|target| target.contains("[#sessiondigest]"))
        );
        assert!(report
            .commands
            .iter()
            .any(|command| command.command == "cargo test --release --manifest-path Cargo.toml"));
        assert!(
            report
                .touched_files
                .iter()
                .any(|path| path.path == "Cargo.toml")
        );
        assert!(
            report
                .touched_symbols
                .iter()
                .any(|symbol| symbol.symbol == "run_sync")
        );
        assert!(
            report
                .failures
                .iter()
                .any(|failure| failure.kind == "missing")
        );
        assert!(report.closeout.iter().any(|entry| entry.kind == "push"));
    }
}
