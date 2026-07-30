//! Whole-tree structural scan and codemod.

use crate::engine::{RewriteOutcome, StructuralMatch, rewrite_source, search_source};
use crate::lang::AstGrepLang;
use anyhow::{Context, Result, bail};
use ignore::WalkBuilder;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// Files above this size are skipped. Structural patterns are for source, and a
/// multi-megabyte generated file costs more to parse than it can ever repay.
const MAX_FILE_BYTES: u64 = 2 * 1024 * 1024;

#[derive(Debug, Clone)]
pub struct ScanOptions {
    /// Roots to walk. Individual file paths are scanned directly.
    pub paths: Vec<PathBuf>,
    /// Force a language instead of inferring it per file extension.
    pub lang: Option<AstGrepLang>,
    /// Cap on reported files; `None` means unbounded.
    pub max_files: Option<usize>,
    /// Honour `.gitignore` and friends while walking.
    pub respect_ignore: bool,
}

impl Default for ScanOptions {
    fn default() -> Self {
        Self {
            paths: vec![PathBuf::from(".")],
            lang: None,
            max_files: None,
            respect_ignore: true,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileMatches {
    pub path: PathBuf,
    pub lang: AstGrepLang,
    pub matches: Vec<StructuralMatch>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScanReport {
    pub pattern: String,
    pub files: Vec<FileMatches>,
    pub files_scanned: usize,
    pub files_skipped_unsupported: usize,
    pub match_count: usize,
    /// Files whose language could not compile this pattern. A mixed-language
    /// tree is the normal case, and one grammar refusing a pattern must not
    /// abort the scan — but the count is reported so a partial sweep never
    /// reads as an exhaustive one.
    pub files_skipped_pattern_unsupported: usize,
    /// Languages behind `files_skipped_pattern_unsupported`, sorted and deduped.
    pub pattern_unsupported_langs: Vec<String>,
    /// True when `max_files` cut the result short — never report a truncated
    /// scan as if it covered the tree.
    pub truncated: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileRewrite {
    pub path: PathBuf,
    pub lang: AstGrepLang,
    pub replacements: usize,
    pub matches: Vec<StructuralMatch>,
    /// Rewritten buffer. Carried even when `applied` is false so callers can
    /// render a preview diff without re-running the rewrite.
    pub new_source: String,
    pub applied: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RewriteReport {
    pub pattern: String,
    pub rewrite: String,
    pub files: Vec<FileRewrite>,
    pub files_scanned: usize,
    pub replacements: usize,
    /// See `ScanReport::files_skipped_pattern_unsupported`.
    pub files_skipped_pattern_unsupported: usize,
    pub pattern_unsupported_langs: Vec<String>,
    pub applied: bool,
    pub truncated: bool,
}

fn collect_candidates(options: &ScanOptions) -> Result<Vec<(PathBuf, AstGrepLang)>> {
    if options.paths.is_empty() {
        bail!("no scan paths supplied");
    }
    let mut out = Vec::new();
    let mut unsupported_explicit = Vec::new();
    for root in &options.paths {
        if !root.exists() {
            bail!("path does not exist: {}", root.display());
        }
        if root.is_file() {
            // An explicitly named file is a user intent, not a walk result, so
            // an unresolvable language is an error rather than a silent skip.
            match options.lang.or_else(|| AstGrepLang::from_path(root)) {
                Some(lang) => out.push((root.clone(), lang)),
                None => unsupported_explicit.push(root.clone()),
            }
            continue;
        }
        let mut walker = WalkBuilder::new(root);
        walker
            .standard_filters(options.respect_ignore)
            .hidden(options.respect_ignore)
            .follow_links(false);
        for entry in walker.build() {
            let entry = entry.with_context(|| format!("walking {}", root.display()))?;
            if !entry.file_type().is_some_and(|ft| ft.is_file()) {
                continue;
            }
            let path = entry.path();
            let Some(lang) = options.lang.or_else(|| AstGrepLang::from_path(path)) else {
                continue;
            };
            if entry
                .metadata()
                .ok()
                .is_some_and(|meta| meta.len() > MAX_FILE_BYTES)
            {
                continue;
            }
            out.push((path.to_path_buf(), lang));
        }
    }
    if !unsupported_explicit.is_empty() {
        let names = unsupported_explicit
            .iter()
            .map(|p| p.display().to_string())
            .collect::<Vec<_>>()
            .join(", ");
        bail!(
            "cannot infer a supported language for: {names} (supported: {}); pass --lang",
            AstGrepLang::supported_names()
        );
    }
    out.sort();
    out.dedup();
    Ok(out)
}

/// Record that `lang` cannot compile the pattern, and keep going.
///
/// A pattern is only ever valid for some grammars. Scanning a mixed tree with
/// `foo($A)` must not fail because one JSON file cannot parse it.
fn note_pattern_unsupported(lang: AstGrepLang, skipped: &mut usize, langs: &mut Vec<String>) {
    *skipped += 1;
    let name = lang.name().to_string();
    if let Err(idx) = langs.binary_search(&name) {
        langs.insert(idx, name);
    }
}

/// A pattern no candidate language could compile is a user error, not an empty
/// result — otherwise a typo reports a confident "0 matches".
fn fail_if_no_language_accepted_the_pattern(
    pattern: &str,
    files_scanned: usize,
    skipped: usize,
    langs: &[String],
) -> Result<()> {
    if files_scanned > 0 && skipped == files_scanned {
        bail!(
            "no scanned language could compile the pattern `{pattern}` (tried: {})",
            langs.join(", ")
        );
    }
    Ok(())
}

fn read_text(path: &Path) -> Option<String> {
    // Binary and non-UTF-8 files are skipped rather than failing the scan.
    std::fs::read(path)
        .ok()
        .and_then(|bytes| String::from_utf8(bytes).ok())
}

/// Structural search across `options.paths`.
pub fn scan(pattern: &str, options: &ScanOptions) -> Result<ScanReport> {
    let candidates = collect_candidates(options)?;
    let mut report = ScanReport {
        pattern: pattern.to_string(),
        files: Vec::new(),
        files_scanned: 0,
        files_skipped_unsupported: 0,
        match_count: 0,
        files_skipped_pattern_unsupported: 0,
        pattern_unsupported_langs: Vec::new(),
        truncated: false,
    };
    for (path, lang) in candidates {
        if let Some(max) = options.max_files
            && report.files.len() >= max
        {
            report.truncated = true;
            break;
        }
        let Some(source) = read_text(&path) else {
            report.files_skipped_unsupported += 1;
            continue;
        };
        report.files_scanned += 1;
        let matches = match search_source(&source, lang, pattern) {
            Ok(matches) => matches,
            Err(_) => {
                note_pattern_unsupported(
                    lang,
                    &mut report.files_skipped_pattern_unsupported,
                    &mut report.pattern_unsupported_langs,
                );
                continue;
            }
        };
        if matches.is_empty() {
            continue;
        }
        report.match_count += matches.len();
        report.files.push(FileMatches {
            path,
            lang,
            matches,
        });
    }
    fail_if_no_language_accepted_the_pattern(
        pattern,
        report.files_scanned,
        report.files_skipped_pattern_unsupported,
        &report.pattern_unsupported_langs,
    )?;
    Ok(report)
}

/// Structural rewrite across `options.paths`.
///
/// `apply` is opt-in: the default is a preview so a codemod is inspected before
/// it touches the working tree.
pub fn codemod(
    pattern: &str,
    rewrite: &str,
    options: &ScanOptions,
    apply: bool,
) -> Result<RewriteReport> {
    let candidates = collect_candidates(options)?;
    let mut report = RewriteReport {
        pattern: pattern.to_string(),
        rewrite: rewrite.to_string(),
        files: Vec::new(),
        files_scanned: 0,
        replacements: 0,
        files_skipped_pattern_unsupported: 0,
        pattern_unsupported_langs: Vec::new(),
        applied: apply,
        truncated: false,
    };
    for (path, lang) in candidates {
        if let Some(max) = options.max_files
            && report.files.len() >= max
        {
            report.truncated = true;
            break;
        }
        let Some(source) = read_text(&path) else {
            continue;
        };
        report.files_scanned += 1;
        let outcome = match rewrite_source(&source, lang, pattern, rewrite) {
            Ok(outcome) => outcome,
            Err(_) => {
                note_pattern_unsupported(
                    lang,
                    &mut report.files_skipped_pattern_unsupported,
                    &mut report.pattern_unsupported_langs,
                );
                continue;
            }
        };
        let RewriteOutcome {
            source: new_source,
            replacements,
            matches,
            unchanged,
        } = outcome;
        if replacements == 0 || unchanged {
            continue;
        }
        if apply {
            std::fs::write(&path, &new_source)
                .with_context(|| format!("writing rewritten {}", path.display()))?;
        }
        report.replacements += replacements;
        report.files.push(FileRewrite {
            path,
            lang,
            replacements,
            matches,
            new_source,
            applied: apply,
        });
    }
    fail_if_no_language_accepted_the_pattern(
        pattern,
        report.files_scanned,
        report.files_skipped_pattern_unsupported,
        &report.pattern_unsupported_langs,
    )?;
    Ok(report)
}

#[cfg(all(test, feature = "lang-rust"))]
mod tests {
    use super::*;
    use std::fs;

    fn fixture() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("a.rs"), "fn a() { foo(1); }\n").unwrap();
        fs::write(dir.path().join("b.rs"), "fn b() { foo(2); foo(3); }\n").unwrap();
        fs::write(dir.path().join("c.txt"), "foo(4);\n").unwrap();
        dir
    }

    fn opts(dir: &tempfile::TempDir) -> ScanOptions {
        ScanOptions {
            paths: vec![dir.path().to_path_buf()],
            ..Default::default()
        }
    }

    #[test]
    fn scans_only_language_files() {
        let dir = fixture();
        let report = scan("foo($A)", &opts(&dir)).unwrap();
        assert_eq!(report.files.len(), 2, "c.txt must not be parsed as Rust");
        assert_eq!(report.match_count, 3);
        assert!(!report.truncated);
    }

    #[test]
    fn preview_does_not_touch_the_working_tree() {
        let dir = fixture();
        let before = fs::read_to_string(dir.path().join("b.rs")).unwrap();
        let report = codemod("foo($A)", "bar($A)", &opts(&dir), false).unwrap();
        assert_eq!(report.replacements, 3);
        assert!(!report.applied);
        assert_eq!(
            fs::read_to_string(dir.path().join("b.rs")).unwrap(),
            before,
            "preview must leave files byte-identical"
        );
        let b = report
            .files
            .iter()
            .find(|f| f.path.ends_with("b.rs"))
            .unwrap();
        assert!(b.new_source.contains("bar(2)") && b.new_source.contains("bar(3)"));
    }

    #[test]
    fn apply_writes_every_matching_file() {
        let dir = fixture();
        let report = codemod("foo($A)", "bar($A)", &opts(&dir), true).unwrap();
        assert!(report.applied);
        assert_eq!(report.replacements, 3);
        assert!(
            fs::read_to_string(dir.path().join("a.rs"))
                .unwrap()
                .contains("bar(1)")
        );
        assert!(
            fs::read_to_string(dir.path().join("b.rs"))
                .unwrap()
                .contains("bar(3)")
        );
        assert_eq!(
            fs::read_to_string(dir.path().join("c.txt")).unwrap(),
            "foo(4);\n",
            "non-source files must be left alone"
        );
    }

    #[test]
    fn max_files_reports_truncation() {
        let dir = fixture();
        let options = ScanOptions {
            max_files: Some(1),
            ..opts(&dir)
        };
        let report = scan("foo($A)", &options).unwrap();
        assert!(report.truncated, "a capped scan must not look exhaustive");
    }

    #[test]
    fn explicit_unsupported_file_is_an_error_not_a_silent_skip() {
        let dir = fixture();
        let options = ScanOptions {
            paths: vec![dir.path().join("c.txt")],
            ..Default::default()
        };
        let err = scan("foo($A)", &options).unwrap_err().to_string();
        assert!(err.contains("--lang"), "got: {err}");
    }

    #[test]
    fn missing_path_is_an_error() {
        let options = ScanOptions {
            paths: vec![PathBuf::from("/definitely/not/here")],
            ..Default::default()
        };
        assert!(scan("foo($A)", &options).is_err());
    }
}
