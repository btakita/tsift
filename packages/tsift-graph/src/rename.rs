//! Identifier occurrence collection for symbol renames.
//!
//! A rename used to be a substring scan with an identifier-boundary guard. That
//! shape cannot tell an identifier from the same characters inside a string
//! literal or a comment, so `rename_symbol` silently rewrote both — a string
//! literal is data, and rewriting it changes behaviour rather than names.
//!
//! Here the walk is restricted to the node kinds that *are* identifiers in each
//! grammar. Comments and string bodies are different node kinds, so they drop
//! out by construction; there is no comment or string special case below, and a
//! new quoting or comment form cannot reintroduce the bug.

use crate::lang::Lang;
use anyhow::Result;
use tree_sitter::{Node, Parser};

/// The byte span of one identifier occurrence, as a half-open range.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IdentifierOccurrence {
    pub start_byte: usize,
    pub end_byte: usize,
}

/// Node kinds that carry a bare identifier in this language's grammar.
///
/// An empty slice means the language has no identifier concept a rename could
/// target (Markdown), which callers must treat as "not renamable" rather than
/// as "no occurrences found".
pub fn identifier_node_kinds(lang: Lang) -> &'static [&'static str] {
    match lang {
        // Rust identifiers inside macro arguments live under an opaque
        // `token_tree`, but they are still named `identifier` nodes, so a
        // `foo()` call inside `assert_eq!`/`format!` is reached by this walk.
        #[cfg(feature = "lang-rust")]
        Lang::Rust => &[
            "identifier",
            "type_identifier",
            "field_identifier",
            "shorthand_field_identifier",
        ],
        #[cfg(feature = "lang-python")]
        Lang::Python => &["identifier"],
        #[cfg(feature = "lang-typescript")]
        Lang::TypeScript | Lang::Tsx => &[
            "identifier",
            "type_identifier",
            "property_identifier",
            "shorthand_property_identifier",
            "shorthand_property_identifier_pattern",
        ],
        #[cfg(feature = "lang-javascript")]
        Lang::JavaScript | Lang::Jsx => &[
            "identifier",
            "property_identifier",
            "shorthand_property_identifier",
            "shorthand_property_identifier_pattern",
        ],
        #[cfg(feature = "lang-kotlin")]
        Lang::Kotlin => &["identifier"],
        #[cfg(feature = "lang-zig")]
        Lang::Zig => &["identifier"],
        // Bash has no separate identifier node: a command name and a function
        // name are both `word`, and an expansion is `variable_name`. `word` is
        // also every unquoted argument, so kind alone is not enough here —
        // `occurrence_is_renamable` narrows it to the name positions.
        #[cfg(feature = "lang-bash")]
        Lang::Bash => &["word", "variable_name"],
        // GDScript splits the two: `name` is the declared name of a statement
        // or block, `identifier` is every reference to one.
        #[cfg(feature = "lang-gdscript")]
        Lang::GdScript => &["identifier", "name"],
        // Markdown has headings, not identifiers; `rename_heading` is its kind.
        #[cfg(feature = "lang-markdown")]
        Lang::Markdown => &[],
    }
}

/// Whether an identifier-kind node sits in a *naming* position.
///
/// For most grammars the node kind settles it, and this is unconditionally
/// true. Bash is the exception that forces the check to exist: a bare `word`
/// is the function name in `deploy() { … }`, the command name in `deploy`,
/// **and** every unquoted argument, so `echo deploy` would otherwise have a
/// rename rewrite an argument that is data. Restricting `word` to the
/// declaration and command-name positions keeps arguments out, the same way
/// the kind filter keeps strings and comments out for every other language.
fn occurrence_is_renamable(lang: Lang, node: Node) -> bool {
    match lang {
        #[cfg(feature = "lang-bash")]
        Lang::Bash => {
            if node.kind() != "word" {
                // `variable_name` is only ever a variable, in an assignment or
                // an expansion.
                return true;
            }
            node.parent().is_some_and(|parent| {
                matches!(parent.kind(), "function_definition" | "command_name")
            })
        }
        _ => {
            let _ = node;
            true
        }
    }
}

/// Every occurrence of `name` that is a real identifier node, in source order.
///
/// Returns an empty vector when the name never appears as an identifier, which
/// is distinct from it appearing only inside strings or comments — both look
/// the same to the caller, and both mean "there is nothing here to rename".
pub fn identifier_occurrences(
    lang: Lang,
    source: &[u8],
    name: &str,
) -> Result<Vec<IdentifierOccurrence>> {
    let kinds = identifier_node_kinds(lang);
    if kinds.is_empty() || name.is_empty() {
        return Ok(Vec::new());
    }

    let ts_lang = lang.tree_sitter_language();
    let mut parser = Parser::new();
    parser.set_language(&ts_lang)?;
    let tree = parser
        .parse(source, None)
        .ok_or_else(|| anyhow::anyhow!("parse failed"))?;

    let mut occurrences = Vec::new();
    let mut cursor = tree.walk();
    let mut descend = true;
    loop {
        if descend {
            let node = cursor.node();
            if kinds.contains(&node.kind())
                && node.utf8_text(source).is_ok_and(|it| it == name)
                && occurrence_is_renamable(lang, node)
            {
                occurrences.push(IdentifierOccurrence {
                    start_byte: node.start_byte(),
                    end_byte: node.end_byte(),
                });
            }
            if cursor.goto_first_child() {
                continue;
            }
        }
        if cursor.goto_next_sibling() {
            descend = true;
            continue;
        }
        if !cursor.goto_parent() {
            break;
        }
        descend = false;
    }

    // A pre-order walk already yields these in source order, but nested
    // grammars can nest an identifier inside another identifier-kind node, and
    // every caller splices spans left to right.
    occurrences.sort_by_key(|occurrence| (occurrence.start_byte, occurrence.end_byte));
    occurrences.dedup();
    Ok(occurrences)
}

/// Splice `replacement` over every occurrence span, returning the new source
/// and the number of substitutions.
pub fn replace_occurrences(
    source: &str,
    occurrences: &[IdentifierOccurrence],
    replacement: &str,
) -> (String, usize) {
    let mut out = String::with_capacity(source.len());
    let mut last = 0usize;
    let mut replaced = 0usize;
    for occurrence in occurrences {
        if occurrence.start_byte < last {
            // Overlapping spans would corrupt the splice; the first one wins.
            continue;
        }
        out.push_str(&source[last..occurrence.start_byte]);
        out.push_str(replacement);
        last = occurrence.end_byte;
        replaced += 1;
    }
    out.push_str(&source[last..]);
    (out, replaced)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(feature = "lang-rust")]
    const RUST_SOURCE: &str = r#"/// doc widget_count
fn widget_count() -> usize { 3 }

fn describe() -> String {
    // widget_count comment
    let label = "widget_count";
    format!("{label}: {}", widget_count())
}
"#;

    #[cfg(feature = "lang-rust")]
    #[test]
    fn rust_skips_strings_and_comments_but_reaches_macro_arguments() {
        let found =
            identifier_occurrences(Lang::Rust, RUST_SOURCE.as_bytes(), "widget_count").unwrap();
        // The definition and the call inside `format!` — not the doc comment,
        // the line comment, or the string literal.
        assert_eq!(
            found.len(),
            2,
            "expected the definition and the macro-argument call, got {found:?}"
        );
        for occurrence in &found {
            let before = &RUST_SOURCE[..occurrence.start_byte];
            assert!(
                !before.ends_with("/// doc ") && !before.ends_with("// "),
                "occurrence at {} is inside a comment",
                occurrence.start_byte
            );
            assert!(
                !before.ends_with('"'),
                "occurrence at {} is inside a string literal",
                occurrence.start_byte
            );
        }
    }

    #[cfg(feature = "lang-rust")]
    #[test]
    fn replacing_rust_occurrences_leaves_prose_and_data_alone() {
        let found =
            identifier_occurrences(Lang::Rust, RUST_SOURCE.as_bytes(), "widget_count").unwrap();
        let (out, replaced) = replace_occurrences(RUST_SOURCE, &found, "gadget_count");
        assert_eq!(replaced, 2);
        assert!(out.contains("fn gadget_count()"), "definition not renamed");
        assert!(
            out.contains("gadget_count())"),
            "macro-argument call not renamed"
        );
        assert!(
            out.contains("/// doc widget_count"),
            "doc comment was renamed"
        );
        assert!(
            out.contains("// widget_count comment"),
            "line comment was renamed"
        );
        assert!(
            out.contains("\"widget_count\""),
            "string literal was renamed"
        );
    }

    #[cfg(feature = "lang-python")]
    #[test]
    fn python_skips_strings_and_comments() {
        let source = "def widget_count():\n    # widget_count comment\n    return \"widget_count\"\n\nwidget_count()\n";
        let found = identifier_occurrences(Lang::Python, source.as_bytes(), "widget_count").unwrap();
        assert_eq!(found.len(), 2, "got {found:?}");
        let (out, replaced) = replace_occurrences(source, &found, "gadget_count");
        assert_eq!(replaced, 2);
        assert!(out.contains("def gadget_count()"));
        assert!(out.contains("gadget_count()\n"));
        assert!(out.contains("# widget_count comment"));
        assert!(out.contains("\"widget_count\""));
    }

    #[cfg(feature = "lang-typescript")]
    #[test]
    fn typescript_skips_strings_and_comments() {
        let source = "// widgetCount comment\nfunction widgetCount(): number { return 1; }\nconst label = \"widgetCount\";\nwidgetCount();\n";
        let found =
            identifier_occurrences(Lang::TypeScript, source.as_bytes(), "widgetCount").unwrap();
        assert_eq!(found.len(), 2, "got {found:?}");
        let (out, _) = replace_occurrences(source, &found, "gadgetCount");
        assert!(out.contains("function gadgetCount()"));
        assert!(out.contains("// widgetCount comment"));
        assert!(out.contains("\"widgetCount\""));
    }

    #[cfg(feature = "lang-bash")]
    const BASH_SOURCE: &str = r#"widget_count() {
  echo widget_count
  local label="widget_count"
  # widget_count comment
  echo "$widget_count"
}
widget_count
"#;

    #[cfg(feature = "lang-bash")]
    #[test]
    fn bash_renames_names_but_not_arguments_prose_or_data() {
        let found =
            identifier_occurrences(Lang::Bash, BASH_SOURCE.as_bytes(), "widget_count").unwrap();
        // The definition, the `$widget_count` expansion, and the bare call —
        // not the `echo widget_count` argument, the string, or the comment.
        assert_eq!(found.len(), 3, "got {found:?}");
        let (out, replaced) = replace_occurrences(BASH_SOURCE, &found, "gadget_count");
        assert_eq!(replaced, 3);
        assert!(out.contains("gadget_count() {"), "definition not renamed");
        assert!(
            out.contains("echo \"$gadget_count\""),
            "expansion not renamed"
        );
        assert!(
            out.contains("}\ngadget_count\n"),
            "bare call not renamed:\n{out}"
        );
        assert!(
            out.contains("echo widget_count\n"),
            "an unquoted argument was renamed, which rewrites data:\n{out}"
        );
        assert!(out.contains("label=\"widget_count\""), "string was renamed");
        assert!(
            out.contains("# widget_count comment"),
            "comment was renamed"
        );
    }

    #[cfg(feature = "lang-zig")]
    #[test]
    fn zig_skips_strings_and_comments() {
        let source = "// widget_count comment\npub fn widget_count() u32 {\n    const label = \"widget_count\";\n    _ = label;\n    return 3;\n}\npub fn caller() u32 { return widget_count(); }\n";
        let found = identifier_occurrences(Lang::Zig, source.as_bytes(), "widget_count").unwrap();
        assert_eq!(found.len(), 2, "got {found:?}");
        let (out, replaced) = replace_occurrences(source, &found, "gadget_count");
        assert_eq!(replaced, 2);
        assert!(out.contains("pub fn gadget_count()"), "definition not renamed");
        assert!(out.contains("return gadget_count();"), "call not renamed");
        assert!(
            out.contains("// widget_count comment"),
            "comment was renamed"
        );
        assert!(out.contains("\"widget_count\""), "string was renamed");
    }

    #[cfg(feature = "lang-gdscript")]
    #[test]
    fn gdscript_renames_declaration_and_reference_but_not_prose() {
        let source = "# widget_count comment\nfunc widget_count():\n\tvar label = \"widget_count\"\n\treturn label\n\nfunc caller():\n\treturn widget_count()\n";
        let found =
            identifier_occurrences(Lang::GdScript, source.as_bytes(), "widget_count").unwrap();
        // GDScript names a declaration with `name` and every reference with
        // `identifier`; the rename has to reach both kinds.
        assert_eq!(found.len(), 2, "got {found:?}");
        let (out, replaced) = replace_occurrences(source, &found, "gadget_count");
        assert_eq!(replaced, 2);
        assert!(out.contains("func gadget_count():"), "definition not renamed");
        assert!(out.contains("return gadget_count()"), "call not renamed");
        assert!(
            out.contains("# widget_count comment"),
            "comment was renamed"
        );
        assert!(out.contains("\"widget_count\""), "string was renamed");
    }

    #[test]
    fn a_name_that_only_appears_in_prose_has_no_occurrences() {
        #[cfg(feature = "lang-rust")]
        {
            let source = "// widget_count\nfn other() {}\n";
            let found =
                identifier_occurrences(Lang::Rust, source.as_bytes(), "widget_count").unwrap();
            assert!(found.is_empty(), "got {found:?}");
        }
    }

    #[cfg(feature = "lang-markdown")]
    #[test]
    fn markdown_has_no_identifier_kinds() {
        assert!(identifier_node_kinds(Lang::Markdown).is_empty());
        assert!(
            identifier_occurrences(Lang::Markdown, b"# widget_count\n", "widget_count")
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn every_indexed_language_declares_its_identifier_kinds() {
        // A `Lang` variant added with no entry here would silently return an
        // empty set and make every rename in that language a no-op.
        for lang in Lang::all() {
            let kinds = identifier_node_kinds(lang);
            if lang.name() == "markdown" {
                continue;
            }
            assert!(
                !kinds.is_empty(),
                "{} declares no identifier node kinds",
                lang.name()
            );
            let ts_lang = lang.tree_sitter_language();
            for kind in kinds {
                assert!(
                    ts_lang.id_for_node_kind(kind, true) != 0,
                    "{} declares node kind {kind:?}, which its grammar does not have",
                    lang.name()
                );
            }
        }
    }
}
