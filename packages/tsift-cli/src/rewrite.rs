use anyhow::{Context as _, Result, bail};
use std::ffi::OsStr;
use std::fs;
use std::io::{BufRead as _, BufReader, Read as _};
use std::path::{Path, PathBuf};
use std::process::Command;

use crate::output::OutputFormat;
use crate::{relativize_pathbuf, shell_quote, shell_split};
use tsift_agent_doc::{session_digest, session_markdown};
use tsift_graph::lang::Lang;
use tsift_quality::lint;

#[derive(Clone, Copy)]
pub(crate) struct OutputCap {
    pub(crate) max_lines: usize,
    pub(crate) strip_prefix: Option<&'static str>,
}

pub(crate) fn execute_rewritten_command(command: &str) -> Result<i32> {
    let effective_command = effective_rewrite_run_command(command);
    let parts = shell_split(&effective_command);
    let Some(program) = parts.first().map(|part| strip_shell_quotes(part)) else {
        bail!("rewritten command was empty");
    };
    let args: Vec<String> = parts[1..]
        .iter()
        .map(|part| strip_shell_quotes(part).to_string())
        .collect();
    let mut command = if program == "tsift" {
        Command::new(std::env::current_exe().context("resolving current tsift executable")?)
    } else {
        Command::new(program)
    };
    let output = command
        .args(&args)
        .output()
        .with_context(|| format!("executing rewritten command `{effective_command}`"))?;

    let stdout = if let Some(cap) = rewrite_output_cap(&effective_command) {
        apply_output_cap(&output.stdout, cap)
    } else {
        String::from_utf8_lossy(&output.stdout).into_owned()
    };
    if !stdout.is_empty() {
        print!("{stdout}");
    }
    if !output.stderr.is_empty() {
        eprint!("{}", String::from_utf8_lossy(&output.stderr));
    }

    Ok(output
        .status
        .code()
        .unwrap_or_else(|| if output.status.success() { 0 } else { 1 }))
}

pub(crate) fn effective_rewrite_run_command(command: &str) -> String {
    let parts = shell_split(command);
    if parts.first().map(|part| strip_shell_quotes(part)) != Some("tsift") {
        return command.to_string();
    }
    let structured = parts
        .iter()
        .skip(1)
        .any(|part| strip_shell_quotes(part) == "--timeout");
    let subcommand = parts
        .iter()
        .skip(1)
        .map(|part| strip_shell_quotes(part))
        .find(|part| !part.starts_with('-'));
    if matches!(subcommand, Some("search")) && !structured {
        format!("{command} --timeout 0")
    } else {
        command.to_string()
    }
}

pub(crate) fn apply_rewrite_output_format(command: &str, format: OutputFormat) -> String {
    let trimmed = command.trim_start();
    let Some(rest) = trimmed.strip_prefix("tsift") else {
        return command.to_string();
    };
    let existing_parts = shell_split(rest);

    let mut flags = Vec::new();
    if format.compact && !rewrite_has_global_flag(&existing_parts, "--compact") {
        flags.push("--compact");
    }
    if format.pretty && !rewrite_has_global_flag(&existing_parts, "--pretty") {
        flags.push("--pretty");
    }
    if format.terse && !rewrite_has_global_flag(&existing_parts, "--terse") {
        flags.push("--terse");
    }
    if format.schema && !rewrite_has_global_flag(&existing_parts, "--schema") {
        flags.push("--schema");
    }
    if format.envelope {
        if !rewrite_has_global_flag(&existing_parts, "--envelope") {
            flags.push("--envelope");
        }
    } else if format.json_output
        && !rewrite_has_global_flag(&existing_parts, "--json")
        && !rewrite_has_global_flag(&existing_parts, "--envelope")
    {
        flags.push("--json");
    }

    if flags.is_empty() {
        return command.to_string();
    }

    let forwarded = flags.join(" ");
    if rest.trim().is_empty() {
        format!("tsift {forwarded}")
    } else {
        format!("tsift {forwarded}{rest}")
    }
}

fn rewrite_has_global_flag(parts: &[&str], flag: &str) -> bool {
    parts
        .iter()
        .take_while(|part| {
            let value = strip_shell_quotes(part);
            value.starts_with('-') || value == "tsift"
        })
        .any(|part| strip_shell_quotes(part) == flag)
}

pub(crate) fn rewrite_output_cap(command: &str) -> Option<OutputCap> {
    let parts = shell_split(command);
    if strip_shell_quotes(parts.first()?) != "tsift" {
        return None;
    }
    let structured = parts.iter().skip(1).any(|part| {
        matches!(
            strip_shell_quotes(part),
            "--json" | "--terse" | "--schema" | "--tabular" | "--envelope"
        )
    });
    if structured {
        return None;
    }

    let subcommand = parts
        .iter()
        .skip(1)
        .map(|part| strip_shell_quotes(part))
        .find(|part| !part.starts_with('-'))?;
    match subcommand {
        "communities" => Some(OutputCap {
            max_lines: 80,
            strip_prefix: None,
        }),
        "explain" => Some(OutputCap {
            max_lines: 40,
            strip_prefix: None,
        }),
        "graph" => Some(OutputCap {
            max_lines: 50,
            strip_prefix: None,
        }),
        "index" => Some(OutputCap {
            max_lines: 30,
            strip_prefix: None,
        }),
        "search" => Some(OutputCap {
            max_lines: 50,
            strip_prefix: Some("Strategy:"),
        }),
        _ => None,
    }
}

pub(crate) fn apply_output_cap(stdout: &[u8], cap: OutputCap) -> String {
    let cleaned = strip_ansi_codes(&String::from_utf8_lossy(stdout));
    let mut lines: Vec<String> = cleaned
        .lines()
        .map(str::trim_end)
        .filter(|line| !line.trim().is_empty())
        .filter(|line| {
            cap.strip_prefix
                .map(|prefix| !line.starts_with(prefix))
                .unwrap_or(true)
        })
        .map(ToOwned::to_owned)
        .collect();
    if lines.len() > cap.max_lines {
        let hidden = lines.len() - cap.max_lines;
        lines.truncate(cap.max_lines);
        lines.push(format!(
            "... (+{hidden} more lines; rerun the underlying tsift command directly for the full output)"
        ));
    }
    if lines.is_empty() {
        String::new()
    } else {
        format!("{}\n", lines.join("\n"))
    }
}

fn strip_ansi_codes(input: &str) -> String {
    let mut output = String::with_capacity(input.len());
    let mut chars = input.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch == '\u{1b}' && matches!(chars.peek(), Some('[')) {
            chars.next();
            for next in chars.by_ref() {
                if ('@'..='~').contains(&next) {
                    break;
                }
            }
            continue;
        }
        output.push(ch);
    }
    output
}

/// Attempt to rewrite a shell command to use tsift.
/// Returns Some(rewritten) if applicable, None if no match.
///
/// `pub` (not `pub(crate)`) so the `tsift-sim-world` test-harness crate can
/// exercise the rewrite surface as a dev-dependency.
pub fn rewrite_command(command: &str) -> Option<String> {
    let trimmed = command.trim();

    // Already a tsift command — pass through (exit 0, identical)
    if trimmed.starts_with("tsift ") || trimmed == "tsift" {
        return Some(command.to_string());
    }

    // rg <pattern> [path] [flags] → tsift search "<pattern>" --exact [--path <path>]
    if let Some(rewritten) = rewrite_rg(trimmed) {
        return Some(rewritten);
    }

    // grep -r <pattern> [path] → tsift search "<pattern>" --exact [--path <path>]
    if let Some(rewritten) = rewrite_grep(trimmed) {
        return Some(rewritten);
    }

    // git diff / git show / patch-style history → tsift diff-digest
    if let Some(rewritten) = rewrite_git_diff(trimmed) {
        return Some(rewritten);
    }
    if let Some(rewritten) = rewrite_git_show(trimmed) {
        return Some(rewritten);
    }
    if let Some(rewritten) = rewrite_git_patch_history(trimmed) {
        return Some(rewritten);
    }

    // recognized session/log reads and indexed source-file reads → bounded digests/windows
    if let Some(rewritten) = rewrite_file_read_command(trimmed) {
        return Some(rewritten);
    }

    // cargo test / pytest → tsift-owned test digest wrapper that preserves exit status
    if let Some(rewritten) = rewrite_test_command(trimmed) {
        return Some(rewritten);
    }

    // verbose build/check/install commands → tsift-owned log digest wrapper
    if let Some(rewritten) = rewrite_log_command(trimmed) {
        return Some(rewritten);
    }

    None
}

pub(crate) fn no_rewrite_message(command: &str, run: bool) -> String {
    let trimmed = command.trim();
    let parts = shell_split(trimmed);
    let reason = if trimmed.is_empty() {
        "empty command".to_string()
    } else if has_shell_metacharacters(trimmed) {
        "shell metacharacters such as pipes, redirection, or background operators are not rewritten"
            .to_string()
    } else if is_file_listing_command(&parts) {
        "file-listing commands keep original shell/find/rg semantics".to_string()
    } else if let Some(selector) = git_show_blob_selector(trimmed) {
        format!(
            "`git show {selector}` reads a blob/tree object, not a commit diff; use the original `git show` or `git cat-file` command"
        )
    } else if let Some(reason) = file_read_decline_reason(trimmed) {
        reason
    } else {
        "no supported tsift rewrite matched this command".to_string()
    };
    let action = if run {
        "`--run` executes only rewritten commands; run the original command directly if intended"
    } else {
        "run the original command unchanged"
    };
    format!("tsift rewrite: no rewrite: {reason}; {action}")
}

fn is_file_listing_command(parts: &[&str]) -> bool {
    match parts.first().copied() {
        Some("find") => true,
        Some("rg") => parts
            .iter()
            .skip(1)
            .any(|part| matches!(*part, "--files" | "--type-list")),
        _ => false,
    }
}

/// Rewrite `rg` (ripgrep) commands to tsift search.
fn rewrite_rg(cmd: &str) -> Option<String> {
    let parts: Vec<&str> = shell_split(cmd);
    if parts.is_empty() || parts[0] != "rg" {
        return None;
    }

    // File-listing forms do not have a search pattern. Leave them to the
    // original command so roots, globs, and ignore rules keep rg semantics.
    if is_file_listing_command(&parts) {
        return None;
    }

    // Skip if rg is used with complex flags we can't translate
    // (pipe chains, output redirection, --replace, --count, etc.)
    if cmd.contains('|')
        || cmd.contains('>')
        || cmd.contains("--replace")
        || cmd.contains("--count")
        || cmd.contains("-c")
        || cmd.contains("--files-with-matches")
        || cmd.contains("--files-without-match")
        || cmd.contains("-l")
    {
        return None;
    }

    // Extract the pattern (first non-flag argument after rg)
    let mut pattern = None;
    let mut path = None;
    let mut skip_next = false;

    for part in &parts[1..] {
        if skip_next {
            skip_next = false;
            continue;
        }
        // Flags that take a value
        if matches!(
            *part,
            "-t" | "--type"
                | "-g"
                | "--glob"
                | "-A"
                | "-B"
                | "-C"
                | "--max-count"
                | "--max-depth"
                | "-m"
                | "-e"
        ) {
            skip_next = true;
            continue;
        }
        // Skip standalone flags
        if part.starts_with('-') {
            continue;
        }
        // First positional = pattern, second = path
        if pattern.is_none() {
            pattern = Some(*part);
        } else if path.is_none() {
            path = Some(*part);
        }
    }

    Some(build_agent_search_preview_command(pattern?, path))
}

/// Rewrite `grep -r` commands to tsift search.
fn rewrite_grep(cmd: &str) -> Option<String> {
    let parts: Vec<&str> = shell_split(cmd);
    if parts.is_empty() || parts[0] != "grep" {
        return None;
    }

    // Only rewrite recursive grep
    let has_recursive = parts.iter().any(|p| {
        *p == "-r"
            || *p == "-R"
            || *p == "--recursive"
            || p.contains('r') && p.starts_with('-') && !p.starts_with("--")
    });
    if !has_recursive {
        return None;
    }

    // Skip pipe chains
    if cmd.contains('|') || cmd.contains('>') {
        return None;
    }

    let mut pattern = None;
    let mut path = None;
    let mut skip_next = false;

    for part in &parts[1..] {
        if skip_next {
            skip_next = false;
            continue;
        }
        if matches!(*part, "--include" | "--exclude" | "--exclude-dir" | "-e") {
            skip_next = true;
            continue;
        }
        if part.starts_with('-') {
            continue;
        }
        if pattern.is_none() {
            pattern = Some(*part);
        } else if path.is_none() {
            path = Some(*part);
        }
    }

    Some(build_agent_search_preview_command(pattern?, path))
}

fn build_agent_search_preview_command(pattern: &str, path: Option<&str>) -> String {
    let mut result = format!(
        "tsift --envelope search {} --exact --budget normal",
        shell_quote(pattern)
    );
    if let Some(p) = path {
        result.push_str(&format!(" --path {}", shell_quote(p)));
    }
    result
}

fn rewrite_git_diff(cmd: &str) -> Option<String> {
    if has_shell_metacharacters(cmd) {
        return None;
    }

    let parts: Vec<&str> = shell_split(cmd);
    if parts.len() < 2 || parts[0] != "git" || parts[1] != "diff" {
        return None;
    }
    let mut cached = false;
    let mut pathspecs = Vec::new();
    let mut after_double_dash = false;

    for part in &parts[2..] {
        if after_double_dash {
            pathspecs.push(*part);
            continue;
        }
        match *part {
            "--cached" | "--staged" => cached = true,
            "--" => after_double_dash = true,
            raw if looks_like_path_selector(raw) => pathspecs.push(raw),
            _ => return None,
        }
    }

    Some(build_diff_digest_command(cached, None, &pathspecs))
}

fn rewrite_git_show(cmd: &str) -> Option<String> {
    if has_shell_metacharacters(cmd) {
        return None;
    }

    let parts: Vec<&str> = shell_split(cmd);
    if parts.len() < 2 || parts[0] != "git" || parts[1] != "show" {
        return None;
    }

    let mut revision = "HEAD";
    let mut revision_set = false;
    let mut pathspecs = Vec::new();
    let mut after_double_dash = false;

    for part in &parts[2..] {
        if after_double_dash {
            pathspecs.push(*part);
            continue;
        }
        match *part {
            "--" => after_double_dash = true,
            "-p" | "--patch" | "--stat" => {}
            raw if raw.starts_with("--format=") => {}
            raw if !raw.starts_with('-') => {
                if revision_set || looks_like_git_blob_selector(raw) {
                    return None;
                }
                revision = raw;
                revision_set = true;
            }
            _ => return None,
        }
    }

    Some(build_diff_digest_command(false, Some(revision), &pathspecs))
}

fn rewrite_git_patch_history(cmd: &str) -> Option<String> {
    if has_shell_metacharacters(cmd) {
        return None;
    }

    let parts: Vec<&str> = shell_split(cmd);
    if parts.len() < 2 || parts[0] != "git" || parts[1] != "log" {
        return None;
    }

    let mut saw_patch = false;
    let mut saw_single_commit = false;
    let mut revision = "HEAD";
    let mut pathspecs = Vec::new();
    let mut after_double_dash = false;
    let mut skip_next = false;

    for part in &parts[2..] {
        if skip_next {
            skip_next = false;
            if *part == "1" {
                saw_single_commit = true;
                continue;
            }
            return None;
        }
        if after_double_dash {
            pathspecs.push(*part);
            continue;
        }
        match *part {
            "--" => after_double_dash = true,
            "-p" | "--patch" => saw_patch = true,
            "-1" | "-n1" | "--max-count=1" => saw_single_commit = true,
            "-n" | "--max-count" => skip_next = true,
            raw if !raw.starts_with('-') => {
                if revision != "HEAD" {
                    return None;
                }
                revision = raw;
            }
            _ => return None,
        }
    }

    if !saw_patch || !saw_single_commit {
        return None;
    }

    Some(build_diff_digest_command(false, Some(revision), &pathspecs))
}

fn build_diff_digest_command(cached: bool, revision: Option<&str>, pathspecs: &[&str]) -> String {
    let mut result = "tsift diff-digest".to_string();
    if cached {
        result.push_str(" --cached");
    }
    if let Some(revision) = revision {
        result.push_str(&format!(" --revision {}", shell_quote(revision)));
    }
    for pathspec in pathspecs {
        result.push_str(&format!(" --pathspec {}", shell_quote(pathspec)));
    }
    result.push_str(" .");
    result
}

const SESSION_READ_LINE_THRESHOLD: usize = 80;
const SOURCE_READ_LINE_THRESHOLD: usize = 80;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FileReadWindow {
    FullFile,
    FromStart { lines: usize },
    FromEnd { lines: usize },
    Range { start: usize, lines: usize },
}

struct FileReadTarget {
    input: String,
    requested_lines: Option<usize>,
    window: FileReadWindow,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum FileReadRewriteDecision {
    Rewritten(String),
    Declined(String),
}

fn rewrite_file_read_command(cmd: &str) -> Option<String> {
    match file_read_rewrite_decision(cmd)? {
        FileReadRewriteDecision::Rewritten(command) => Some(command),
        FileReadRewriteDecision::Declined(_) => None,
    }
}

fn file_read_decline_reason(cmd: &str) -> Option<String> {
    match file_read_rewrite_decision(cmd)? {
        FileReadRewriteDecision::Rewritten(_) => None,
        FileReadRewriteDecision::Declined(reason) => Some(reason),
    }
}

fn file_read_rewrite_decision(cmd: &str) -> Option<FileReadRewriteDecision> {
    if has_shell_metacharacters(cmd) {
        return None;
    }

    let target = parse_file_read_target(cmd)?;
    let input_path = expand_tilde_path(&target.input);
    if !input_path.is_file() {
        return Some(FileReadRewriteDecision::Declined(format!(
            "read target `{}` is not a readable regular file",
            target.input
        )));
    }

    let explicit_window = target.requested_lines.is_some();
    if let Some(source) = detect_session_digest_source(&input_path) {
        if !explicit_window && !file_has_at_least_lines(&input_path, SESSION_READ_LINE_THRESHOLD) {
            return Some(FileReadRewriteDecision::Declined(format!(
                "recognized session input `{}` is below the {}-line whole-file rewrite threshold",
                target.input, SESSION_READ_LINE_THRESHOLD
            )));
        }
        let digest_path = resolve_digest_context_path(&input_path);
        return Some(FileReadRewriteDecision::Rewritten(
            build_session_digest_command(&digest_path, &input_path.to_string_lossy(), source),
        ));
    }

    if file_looks_like_captured_log(&input_path) {
        if !explicit_window && !file_has_at_least_lines(&input_path, SESSION_READ_LINE_THRESHOLD) {
            return Some(FileReadRewriteDecision::Declined(format!(
                "log input `{}` is below the {}-line whole-file rewrite threshold",
                target.input, SESSION_READ_LINE_THRESHOLD
            )));
        }
        return Some(FileReadRewriteDecision::Rewritten(
            build_log_digest_read_command(&input_path),
        ));
    }

    if !file_is_supported_source(&input_path) {
        return Some(FileReadRewriteDecision::Declined(format!(
            "read target `{}` is not a recognized source, session, or log input",
            target.input
        )));
    }

    let root = match lint::find_project_root_for_path(&input_path) {
        Ok(Some(root)) => root,
        _ => {
            return Some(FileReadRewriteDecision::Declined(format!(
                "no project root found for source read `{}`",
                target.input
            )));
        }
    };
    if !project_has_index(&root) {
        return Some(FileReadRewriteDecision::Declined(format!(
            "no index coverage for `{}` (resolved project root `{}` has no index.db; run `tsift index {}`)",
            target.input,
            root.display(),
            root.display()
        )));
    }
    let file_abs = input_path.canonicalize().ok()?;
    let file_display = relativize_pathbuf(&file_abs, &root)
        .to_string_lossy()
        .to_string();
    let total_lines = count_file_lines(&file_abs)?;
    if !explicit_window && total_lines < SOURCE_READ_LINE_THRESHOLD {
        return Some(FileReadRewriteDecision::Declined(format!(
            "source read `{}` is below the {}-line whole-file rewrite threshold",
            target.input, SOURCE_READ_LINE_THRESHOLD
        )));
    }
    let Some((start, lines)) = source_window_for_read(target.window, total_lines) else {
        return Some(FileReadRewriteDecision::Declined(format!(
            "requested source window for `{}` is outside the file's {} lines",
            target.input, total_lines
        )));
    };
    Some(FileReadRewriteDecision::Rewritten(
        build_source_read_rewrite_command(&root, &file_display, start, lines),
    ))
}

fn parse_file_read_target(cmd: &str) -> Option<FileReadTarget> {
    let parts: Vec<&str> = shell_split(cmd);
    let head = parts.first().copied()?;
    match head {
        "cat" | "bat" | "batcat" | "less" => parse_cat_like_read_target(&parts),
        "head" | "tail" => parse_head_tail_read_target(&parts),
        "sed" => parse_sed_read_target(&parts),
        _ => None,
    }
}

fn parse_cat_like_read_target(parts: &[&str]) -> Option<FileReadTarget> {
    let mut input = None;
    for part in &parts[1..] {
        if part.starts_with('-') {
            continue;
        }
        if input.replace(strip_shell_quotes(part)).is_some() {
            return None;
        }
    }
    Some(FileReadTarget {
        input: input?.to_string(),
        requested_lines: None,
        window: FileReadWindow::FullFile,
    })
}

fn parse_head_tail_read_target(parts: &[&str]) -> Option<FileReadTarget> {
    let mut requested_lines = 10;
    let mut input = None;
    let mut index = 1;

    while index < parts.len() {
        let part = parts[index];
        if part == "-n" || part == "--lines" {
            index += 1;
            requested_lines = parse_requested_line_count(parts.get(index).copied()?)?;
            index += 1;
            continue;
        }
        if let Some(raw) = part.strip_prefix("-n")
            && !raw.is_empty()
        {
            requested_lines = parse_requested_line_count(raw)?;
            index += 1;
            continue;
        }
        if let Some(raw) = part.strip_prefix("--lines=") {
            requested_lines = parse_requested_line_count(raw)?;
            index += 1;
            continue;
        }
        if part.starts_with('-') && part[1..].chars().all(|ch| ch.is_ascii_digit()) {
            requested_lines = parse_requested_line_count(&part[1..])?;
            index += 1;
            continue;
        }
        if input.replace(strip_shell_quotes(part)).is_some() {
            return None;
        }
        index += 1;
    }

    let window = match parts[0] {
        "head" => FileReadWindow::FromStart {
            lines: requested_lines,
        },
        "tail" => FileReadWindow::FromEnd {
            lines: requested_lines,
        },
        _ => return None,
    };

    Some(FileReadTarget {
        input: input?.to_string(),
        requested_lines: Some(requested_lines),
        window,
    })
}

fn parse_sed_read_target(parts: &[&str]) -> Option<FileReadTarget> {
    if parts.len() != 4 || parts[1] != "-n" {
        return None;
    }

    let (start, lines) = parse_sed_print_window(parts[2])?;
    Some(FileReadTarget {
        input: strip_shell_quotes(parts[3]).to_string(),
        requested_lines: Some(lines),
        window: FileReadWindow::Range { start, lines },
    })
}

fn parse_requested_line_count(raw: &str) -> Option<usize> {
    let trimmed = strip_shell_quotes(raw);
    trimmed.parse::<usize>().ok()
}

pub(crate) fn expand_tilde_path(input: &str) -> PathBuf {
    if input == "~" {
        return std::env::var_os("HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from(input));
    }
    if let Some(relative) = input.strip_prefix("~/")
        && let Some(home) = std::env::var_os("HOME")
    {
        return PathBuf::from(home).join(relative);
    }
    PathBuf::from(input)
}

fn parse_sed_print_window(raw: &str) -> Option<(usize, usize)> {
    let trimmed = strip_shell_quotes(raw);
    let range = trimmed.strip_suffix('p')?;
    let (start, end) = range.split_once(',')?;
    let start = start.parse::<usize>().ok()?;
    let end = end.parse::<usize>().ok()?;
    (end >= start).then_some((start, end - start + 1))
}

fn file_is_supported_source(path: &Path) -> bool {
    path.extension()
        .and_then(|ext| ext.to_str())
        .and_then(Lang::from_extension)
        .is_some()
}

fn count_file_lines(path: &Path) -> Option<usize> {
    let file = fs::File::open(path).ok()?;
    Some(
        BufReader::new(file)
            .lines()
            .filter(|line| line.is_ok())
            .count(),
    )
}

fn source_window_for_read(window: FileReadWindow, total_lines: usize) -> Option<(usize, usize)> {
    if total_lines == 0 {
        return None;
    }
    match window {
        FileReadWindow::FullFile => Some((1, SOURCE_READ_LINE_THRESHOLD.min(total_lines))),
        FileReadWindow::FromStart { lines } => Some((1, lines.min(total_lines))),
        FileReadWindow::FromEnd { lines } => {
            let bounded = lines.min(total_lines);
            Some((total_lines - bounded + 1, bounded))
        }
        FileReadWindow::Range { start, lines } => {
            if start == 0 || start > total_lines {
                return None;
            }
            Some((start, lines.min(total_lines - start + 1)))
        }
    }
}

fn build_source_read_rewrite_command(
    root: &Path,
    file: &str,
    start: usize,
    lines: usize,
) -> String {
    format!(
        "tsift --envelope source-read {} --path {} --style window --start {} --lines {} --budget normal",
        shell_quote(file),
        shell_quote(&root.to_string_lossy()),
        start,
        lines
    )
}

fn project_has_index(root: &Path) -> bool {
    let tsift_dir = root.join(".tsift");
    tsift_dir.join("index.db").is_file() || directory_contains_index_db(&tsift_dir.join("indexes"))
}

fn directory_contains_index_db(path: &Path) -> bool {
    let Ok(entries) = fs::read_dir(path) else {
        return false;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.file_name().is_some_and(|name| name == "index.db") && path.is_file() {
            return true;
        }
        if path.is_dir() && directory_contains_index_db(&path) {
            return true;
        }
    }
    false
}

fn detect_session_digest_source(path: &Path) -> Option<session_digest::SessionDigestSource> {
    match path.extension().and_then(|ext| ext.to_str()) {
        Some("md") if session_markdown::markdown_file_looks_like_agent_doc_session(path) => {
            Some(session_digest::SessionDigestSource::Markdown)
        }
        Some("jsonl")
            if path_has_component_pair(path, ".claude", "projects")
                || file_looks_like_claude_jsonl(path) =>
        {
            Some(session_digest::SessionDigestSource::ClaudeJsonl)
        }
        Some("jsonl")
            if path_has_component_pair(path, ".codex", "sessions")
                || file_looks_like_codex_jsonl(path) =>
        {
            Some(session_digest::SessionDigestSource::CodexJsonl)
        }
        Some("log") if session_markdown::log_file_looks_like_agent_doc_runtime_log(path) => {
            Some(session_digest::SessionDigestSource::AgentDocLog)
        }
        _ => None,
    }
}

fn path_has_component_pair(path: &Path, first: &str, second: &str) -> bool {
    let components = path.components().collect::<Vec<_>>();
    components.windows(2).any(|pair| {
        pair[0].as_os_str() == OsStr::new(first) && pair[1].as_os_str() == OsStr::new(second)
    })
}

fn file_looks_like_captured_log(path: &Path) -> bool {
    let Some(file_name) = path.file_name().and_then(OsStr::to_str) else {
        return false;
    };
    let file_name = file_name.to_ascii_lowercase();
    matches!(
        path.extension()
            .and_then(OsStr::to_str)
            .map(str::to_ascii_lowercase)
            .as_deref(),
        Some("log" | "out")
    ) || file_name.ends_with(".output.txt")
        || file_name.ends_with(".log.txt")
}

fn file_looks_like_claude_jsonl(path: &Path) -> bool {
    let prefix = match read_file_prefix(path, 16 * 1024) {
        Some(prefix) => prefix,
        None => return false,
    };

    prefix
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .take(3)
        .any(|line| {
            let value = match serde_json::from_str::<serde_json::Value>(line) {
                Ok(value) => value,
                Err(_) => return false,
            };
            value.get("message").is_some()
                || value.get("role").is_some()
                || value.get("content").is_some()
        })
}

fn file_looks_like_codex_jsonl(path: &Path) -> bool {
    let prefix = match read_file_prefix(path, 16 * 1024) {
        Some(prefix) => prefix,
        None => return false,
    };

    prefix
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .take(8)
        .any(|line| {
            let value = match serde_json::from_str::<serde_json::Value>(line) {
                Ok(value) => value,
                Err(_) => return false,
            };
            matches!(
                value.get("type").and_then(serde_json::Value::as_str),
                Some("session_meta" | "response_item" | "event_msg")
            )
        })
}

fn read_file_prefix(path: &Path, max_bytes: usize) -> Option<String> {
    let file = fs::File::open(path).ok()?;
    let mut reader = BufReader::new(file);
    let mut buffer = Vec::new();
    reader
        .by_ref()
        .take(max_bytes as u64)
        .read_to_end(&mut buffer)
        .ok()?;
    Some(String::from_utf8_lossy(&buffer).into_owned())
}

fn file_has_at_least_lines(path: &Path, min_lines: usize) -> bool {
    let file = match fs::File::open(path) {
        Ok(file) => file,
        Err(_) => return false,
    };
    let reader = BufReader::new(file);
    reader
        .lines()
        .take(min_lines)
        .filter(|line| line.is_ok())
        .count()
        >= min_lines
}

fn build_session_digest_command(
    path: &str,
    input: &str,
    source: session_digest::SessionDigestSource,
) -> String {
    format!(
        "tsift session-digest --path {} --input {} --source {}",
        shell_quote(path),
        shell_quote(input),
        source.cli_arg()
    )
}

fn build_log_digest_read_command(input: &Path) -> String {
    format!(
        "tsift log-digest --path {} --input {}",
        shell_quote(&resolve_digest_context_path(input)),
        shell_quote(&input.to_string_lossy())
    )
}

pub(crate) fn resolve_digest_context_path(path: &Path) -> String {
    lint::resolve_harness_root_or_canonical_path(path)
        .map(|root| root.display().to_string())
        .unwrap_or_else(|_| ".".to_string())
}

fn rewrite_test_command(cmd: &str) -> Option<String> {
    if has_shell_metacharacters(cmd) {
        return None;
    }

    let parts: Vec<&str> = shell_split(cmd);
    if parts.len() >= 2 && parts[0] == "cargo" && parts[1] == "test" {
        return Some(build_digest_runner_command("test", ".", Some("cargo"), cmd));
    }
    if !parts.is_empty() && parts[0] == "pytest" {
        return Some(build_digest_runner_command(
            "test",
            ".",
            Some("pytest"),
            cmd,
        ));
    }
    if parts.len() >= 3 && parts[0] == "python" && parts[1] == "-m" && parts[2] == "pytest" {
        return Some(build_digest_runner_command(
            "test",
            ".",
            Some("pytest"),
            cmd,
        ));
    }
    None
}

fn rewrite_log_command(cmd: &str) -> Option<String> {
    if has_shell_metacharacters(cmd) {
        return None;
    }

    let parts: Vec<&str> = shell_split(cmd);
    if parts.len() >= 2
        && parts[0] == "cargo"
        && matches!(parts[1], "build" | "check" | "clippy" | "install")
    {
        return Some(build_digest_runner_command("log", ".", None, cmd));
    }
    None
}

fn build_digest_runner_command(
    kind: &str,
    path: &str,
    runner: Option<&str>,
    shell_command: &str,
) -> String {
    let mut result = format!(
        "tsift --envelope digest-runner --kind {} --path {} --shell-command {}",
        shell_quote(kind),
        shell_quote(path),
        shell_quote(shell_command)
    );
    if let Some(runner) = runner {
        result.push_str(&format!(" --runner {}", shell_quote(runner)));
    }
    result
}

fn has_shell_metacharacters(cmd: &str) -> bool {
    cmd.contains('|') || cmd.contains('>') || cmd.contains('<') || cmd.contains('&')
}

fn strip_shell_quotes(s: &str) -> &str {
    if s.len() >= 2
        && ((s.starts_with('"') && s.ends_with('"')) || (s.starts_with('\'') && s.ends_with('\'')))
    {
        &s[1..s.len() - 1]
    } else {
        s
    }
}

fn looks_like_path_selector(raw: &str) -> bool {
    raw.ends_with('/')
        || raw.starts_with("./")
        || raw.starts_with("../")
        || raw.contains('/')
        || raw.contains('.')
}

fn looks_like_git_blob_selector(raw: &str) -> bool {
    raw.split_once(':')
        .is_some_and(|(_, path)| !path.is_empty())
}

fn git_show_blob_selector(command: &str) -> Option<String> {
    let parts = shell_split(command);
    if parts.len() < 3 || parts[0] != "git" || parts[1] != "show" {
        return None;
    }
    for part in &parts[2..] {
        if *part == "--" {
            break;
        }
        if !part.starts_with('-') && looks_like_git_blob_selector(part) {
            return Some((*part).to_string());
        }
    }
    None
}
