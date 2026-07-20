//! `tsift ast-grep` — structural search and rewrite.
//!
//! Complements index-backed `tsift search`: patterns match code *shape*
//! (`foo($A)`, `if $C { $$$B }`) rather than tokens, and the same pattern
//! language drives the codemod path.

use crate::output::{OutputFormat, ResponseBudget, ToolEnvelopeSummary};
use crate::{envelope_metric, print_json_or_envelope};
use anyhow::{Result, bail};
use serde::Serialize;
use std::path::PathBuf;
use tsift_astgrep::{AstGrepLang, RewriteReport, ScanOptions, ScanReport, codemod, scan};

fn resolve_lang(lang: Option<&str>) -> Result<Option<AstGrepLang>> {
    let Some(name) = lang else {
        return Ok(None);
    };
    match AstGrepLang::from_name(name) {
        Some(lang) => Ok(Some(lang)),
        None => bail!(
            "unsupported --lang '{name}' (supported: {})",
            AstGrepLang::supported_names()
        ),
    }
}

fn build_options(
    paths: Vec<PathBuf>,
    lang: Option<&str>,
    no_ignore: bool,
    budget: ResponseBudget,
) -> Result<ScanOptions> {
    let paths = if paths.is_empty() {
        vec![PathBuf::from(".")]
    } else {
        paths
    };
    Ok(ScanOptions {
        paths,
        lang: resolve_lang(lang)?,
        // Only a preview run is capped; an unbudgeted run must stay exhaustive
        // so a codemod never silently skips files it was asked to change.
        max_files: budget.is_active().then(|| budget.preview_items()),
        respect_ignore: !no_ignore,
    })
}

#[derive(Serialize)]
struct LanguagesReport {
    languages: Vec<&'static str>,
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn cmd_ast_grep_search(
    pattern: &str,
    paths: Vec<PathBuf>,
    lang: Option<&str>,
    no_ignore: bool,
    format: OutputFormat,
    budget: ResponseBudget,
) -> Result<()> {
    let options = build_options(paths, lang, no_ignore, budget)?;
    let report = scan(pattern, &options)?;

    if format.json_output {
        let summary = ToolEnvelopeSummary {
            text: format!(
                "structural search `{pattern}` — {} match(es) in {} file(s)",
                report.match_count,
                report.files.len()
            ),
            metrics: vec![
                envelope_metric("matches", report.match_count),
                envelope_metric("files", report.files.len()),
                envelope_metric("files_scanned", report.files_scanned),
            ],
        };
        return print_json_or_envelope(
            &report,
            &format,
            "ast-grep",
            "search",
            summary,
            report.truncated,
            follow_up_for_search(pattern, &report),
        );
    }

    print_search_text(&report);
    Ok(())
}

fn follow_up_for_search(pattern: &str, report: &ScanReport) -> Vec<String> {
    let mut out = Vec::new();
    if report.truncated {
        out.push(format!(
            "tsift ast-grep search '{pattern}' --json  # unbudgeted, exhaustive"
        ));
    }
    if report.match_count > 0 {
        out.push(format!(
            "tsift ast-grep rewrite '{pattern}' '<replacement>'  # preview a codemod"
        ));
    }
    out
}

fn print_search_text(report: &ScanReport) {
    for file in &report.files {
        for m in &file.matches {
            println!(
                "{}:{}:{}: {}",
                file.path.display(),
                m.start_line,
                m.start_column,
                m.text.lines().next().unwrap_or_default()
            );
        }
    }
    println!(
        "{} match(es) in {} of {} scanned file(s){}",
        report.match_count,
        report.files.len(),
        report.files_scanned,
        if report.truncated {
            " [truncated by budget]"
        } else {
            ""
        }
    );
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn cmd_ast_grep_rewrite(
    pattern: &str,
    rewrite: &str,
    paths: Vec<PathBuf>,
    lang: Option<&str>,
    no_ignore: bool,
    apply: bool,
    format: OutputFormat,
    budget: ResponseBudget,
) -> Result<()> {
    let options = build_options(paths, lang, no_ignore, budget)?;
    if apply && options.max_files.is_some() {
        // Applying a capped scan would write a partial codemod and report it as
        // done. Refuse rather than half-apply.
        bail!("--apply cannot run under a preview budget; drop --max-items/--budget");
    }
    let report = codemod(pattern, rewrite, &options, apply)?;

    if format.json_output {
        let summary = ToolEnvelopeSummary {
            text: format!(
                "structural rewrite `{pattern}` -> `{rewrite}` — {} replacement(s) in {} file(s) ({})",
                report.replacements,
                report.files.len(),
                if apply { "applied" } else { "preview" }
            ),
            metrics: vec![
                envelope_metric("replacements", report.replacements),
                envelope_metric("files", report.files.len()),
                envelope_metric("applied", report.applied),
            ],
        };
        return print_json_or_envelope(
            &report,
            &format,
            "ast-grep",
            "rewrite",
            summary,
            report.truncated,
            follow_up_for_rewrite(pattern, rewrite, &report),
        );
    }

    print_rewrite_text(&report);
    Ok(())
}

fn follow_up_for_rewrite(pattern: &str, rewrite: &str, report: &RewriteReport) -> Vec<String> {
    if report.applied || report.replacements == 0 {
        Vec::new()
    } else {
        vec![format!(
            "tsift ast-grep rewrite '{pattern}' '{rewrite}' --apply  # write to disk"
        )]
    }
}

fn print_rewrite_text(report: &RewriteReport) {
    for file in &report.files {
        println!(
            "{}: {} replacement(s)",
            file.path.display(),
            file.replacements
        );
        for m in &file.matches {
            println!("  {}:{}: {}", m.start_line, m.start_column, m.text);
        }
    }
    println!(
        "{} replacement(s) in {} of {} scanned file(s) [{}]",
        report.replacements,
        report.files.len(),
        report.files_scanned,
        if report.applied {
            "applied"
        } else {
            "preview — re-run with --apply to write"
        }
    );
}

pub(crate) fn cmd_ast_grep_languages(format: OutputFormat) -> Result<()> {
    let report = LanguagesReport {
        languages: AstGrepLang::all().iter().map(|l| l.name()).collect(),
    };
    if format.json_output {
        let summary = ToolEnvelopeSummary {
            text: format!(
                "{} structural language(s) compiled into this build",
                report.languages.len()
            ),
            metrics: vec![envelope_metric("languages", report.languages.len())],
        };
        return print_json_or_envelope(
            &report,
            &format,
            "ast-grep",
            "languages",
            summary,
            false,
            Vec::new(),
        );
    }
    for lang in &report.languages {
        println!("{lang}");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::output::ResponseBudgetPreset;

    #[test]
    fn unknown_language_is_rejected_with_the_supported_set() {
        let err = resolve_lang(Some("cobol")).unwrap_err().to_string();
        assert!(err.contains("unsupported --lang 'cobol'"), "got: {err}");
        assert!(err.contains("supported:"), "got: {err}");
    }

    #[test]
    fn empty_paths_default_to_current_directory() {
        let options = build_options(vec![], None, false, ResponseBudget::default()).unwrap();
        assert_eq!(options.paths, vec![PathBuf::from(".")]);
    }

    #[test]
    fn unbudgeted_scan_is_uncapped() {
        let options = build_options(vec![], None, false, ResponseBudget::default()).unwrap();
        assert!(
            options.max_files.is_none(),
            "an unbudgeted scan must stay exhaustive"
        );
    }

    #[test]
    fn preview_budget_caps_file_count() {
        let budget = ResponseBudget::from_cli(Some(3), None, Some(ResponseBudgetPreset::Small), false);
        let options = build_options(vec![], None, false, budget).unwrap();
        assert_eq!(options.max_files, Some(3));
    }

    #[test]
    fn no_ignore_flips_the_walker_filter() {
        let options = build_options(vec![], None, true, ResponseBudget::default()).unwrap();
        assert!(!options.respect_ignore);
    }
}
