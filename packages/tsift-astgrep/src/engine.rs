//! Structural match + rewrite over a single source buffer.

use crate::lang::AstGrepLang;
use anyhow::{Result, bail};
use ast_grep_core::{AstGrep, Pattern};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// One structural match, located in both byte and line/column space so callers
/// can cite it the same way tsift cites index-backed hits.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StructuralMatch {
    pub start_byte: usize,
    pub end_byte: usize,
    /// 1-based, matching editor and `file:line:column` conventions.
    pub start_line: usize,
    pub start_column: usize,
    pub end_line: usize,
    pub end_column: usize,
    pub text: String,
    /// Metavariable captures (`$A` -> matched text), single metavariables only.
    pub captures: BTreeMap<String, String>,
}

/// Result of applying a rewrite to one buffer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RewriteOutcome {
    pub source: String,
    pub replacements: usize,
    pub matches: Vec<StructuralMatch>,
    /// True when the rewrite produced no textual change even though the pattern
    /// matched — a no-op codemod, which is usually a pattern/rewrite mistake.
    pub unchanged: bool,
}

fn empty_pattern_error(pattern: &str) -> Result<()> {
    if pattern.trim().is_empty() {
        bail!("structural pattern is empty");
    }
    Ok(())
}

/// Compile a pattern against one grammar, as a refusal rather than a panic.
///
/// `ast-grep-core`'s `&str`-as-matcher path goes through `Pattern::new`, which
/// `unwrap()`s the parse result — so a pattern that is merely invalid for the
/// selected grammar (two statements, an empty parse, a bare `$$$ARGS`) aborts
/// the whole process instead of returning an error. Every language is affected,
/// and the pattern comes straight from the user. `try_new` is the same parse
/// with the error preserved.
fn compile_pattern(pattern: &str, lang: AstGrepLang) -> Result<Pattern> {
    empty_pattern_error(pattern)?;
    Pattern::try_new(pattern, lang.support_lang()).map_err(|err| {
        anyhow::anyhow!(
            "invalid structural pattern for {lang}: {err} (pattern: `{pattern}`)",
            lang = lang.name()
        )
    })
}

/// Find every non-overlapping match of `pattern` in `source`.
pub fn search_source(
    source: &str,
    lang: AstGrepLang,
    pattern: &str,
) -> Result<Vec<StructuralMatch>> {
    let pattern = compile_pattern(pattern, lang)?;
    let grep = AstGrep::new(source, lang.support_lang());
    Ok(collect_matches(&grep, &pattern))
}

fn collect_matches(
    grep: &AstGrep<ast_grep_core::tree_sitter::StrDoc<ast_grep_language::SupportLang>>,
    pattern: &Pattern,
) -> Vec<StructuralMatch> {
    let mut out = Vec::new();
    for matched in grep.root().find_all(pattern) {
        let range = matched.range();
        let start = matched.start_pos();
        let end = matched.end_pos();
        let mut captures = BTreeMap::new();
        for (name, node) in matched.get_env().get_matched_variables().filter_map(|var| {
            // Only single metavariables carry a single node of text; multi
            // metavariables (`$$$ARGS`) are reported through the match text.
            match var {
                ast_grep_core::meta_var::MetaVariable::Capture(name, _) => matched
                    .get_env()
                    .get_match(&name)
                    .map(|node| (name, node.clone())),
                _ => None,
            }
        }) {
            captures.insert(name, node.text().to_string());
        }
        out.push(StructuralMatch {
            start_byte: range.start,
            end_byte: range.end,
            start_line: start.line() + 1,
            start_column: start.column(&matched) + 1,
            end_line: end.line() + 1,
            end_column: end.column(&matched) + 1,
            text: matched.text().to_string(),
            captures,
        });
    }
    out
}

/// Rewrite every non-overlapping match of `pattern` with `rewrite`.
///
/// Edits arrive in source order and are applied back-to-front so earlier byte
/// offsets stay valid — applying front-to-back would shift every subsequent
/// range by the accumulated length delta.
pub fn rewrite_source(
    source: &str,
    lang: AstGrepLang,
    pattern: &str,
    rewrite: &str,
) -> Result<RewriteOutcome> {
    let pattern = compile_pattern(pattern, lang)?;
    // One parse serves both the match report and the edit list; matching and
    // rewriting must never disagree about what the pattern hit.
    let grep = AstGrep::new(source, lang.support_lang());
    let matches = collect_matches(&grep, &pattern);
    let edits = grep.root().replace_all(&pattern, rewrite);

    let mut buffer = source.to_string();
    for edit in edits.iter().rev() {
        let start = edit.position;
        let end = edit.position + edit.deleted_length;
        if end > buffer.len() || !buffer.is_char_boundary(start) || !buffer.is_char_boundary(end) {
            bail!("structural rewrite produced an edit outside a UTF-8 boundary at byte {start}");
        }
        let inserted = String::from_utf8(edit.inserted_text.clone())
            .map_err(|err| anyhow::anyhow!("rewrite produced invalid UTF-8: {err}"))?;
        buffer.replace_range(start..end, &inserted);
    }

    let replacements = edits.len();
    Ok(RewriteOutcome {
        unchanged: replacements > 0 && buffer == source,
        source: buffer,
        replacements,
        matches,
    })
}

#[cfg(all(test, feature = "lang-rust"))]
mod tests {
    use super::*;

    const SRC: &str = "fn main() {\n    foo(1);\n    foo(2);\n}\n";

    #[test]
    fn finds_all_matches_with_positions_and_captures() {
        let hits = search_source(SRC, AstGrepLang::Rust, "foo($A)").unwrap();
        assert_eq!(hits.len(), 2);
        assert_eq!(hits[0].start_line, 2);
        assert_eq!(hits[0].start_column, 5);
        assert_eq!(hits[0].end_column, 11);
        assert_eq!(hits[0].text, "foo(1)");
        assert_eq!(hits[0].captures.get("A").map(String::as_str), Some("1"));
        assert_eq!(hits[1].start_line, 3);
        assert_eq!(hits[1].captures.get("A").map(String::as_str), Some("2"));
    }

    #[test]
    fn rewrites_every_match_not_just_the_first() {
        // The one-shot `AstGrep::replace` only edits the first match; this is
        // the regression that guards the replace_all + reverse-apply path.
        let out = rewrite_source(SRC, AstGrepLang::Rust, "foo($A)", "bar($A)").unwrap();
        assert_eq!(out.replacements, 2);
        assert!(out.source.contains("bar(1)"));
        assert!(out.source.contains("bar(2)"));
        assert!(!out.source.contains("foo("));
        assert!(!out.unchanged);
    }

    #[test]
    fn rewrite_that_grows_text_keeps_later_offsets_valid() {
        // Back-to-front application matters most when the replacement is longer
        // than the match; front-to-back would corrupt the second edit.
        let out = rewrite_source(
            SRC,
            AstGrepLang::Rust,
            "foo($A)",
            "some_much_longer_name($A, extra)",
        )
        .unwrap();
        assert_eq!(out.replacements, 2);
        assert!(out.source.contains("some_much_longer_name(1, extra)"));
        assert!(out.source.contains("some_much_longer_name(2, extra)"));
    }

    #[test]
    fn no_match_leaves_source_untouched() {
        let out = rewrite_source(SRC, AstGrepLang::Rust, "nope($A)", "yes($A)").unwrap();
        assert_eq!(out.replacements, 0);
        assert_eq!(out.source, SRC);
        assert!(!out.unchanged, "no-match is not a no-op rewrite");
    }

    #[test]
    fn identity_rewrite_is_flagged_unchanged() {
        let out = rewrite_source(SRC, AstGrepLang::Rust, "foo($A)", "foo($A)").unwrap();
        assert_eq!(out.replacements, 2);
        assert!(out.unchanged);
    }

    #[test]
    fn multibyte_source_survives_rewrite() {
        let src = "fn main() {\n    let s = \"héllo → wörld\";\n    foo(s);\n}\n";
        let out = rewrite_source(src, AstGrepLang::Rust, "foo($A)", "bar($A)").unwrap();
        assert!(out.source.contains("héllo → wörld"));
        assert!(out.source.contains("bar(s)"));
    }

    #[test]
    fn empty_pattern_is_rejected() {
        assert!(search_source(SRC, AstGrepLang::Rust, "   ").is_err());
        assert!(rewrite_source(SRC, AstGrepLang::Rust, "", "x").is_err());
    }
}
