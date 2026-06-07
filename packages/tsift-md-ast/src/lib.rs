//! Focused markdown AST extraction built on `tree-sitter-md`.
//!
//! This is a deliberately small **leaf** crate (`#tsift-shared-md-ast-and-memgraphrag-lib`):
//! it owns the markdown grammar and the heading/section/code-block/list model so
//! that both `tsift-graph` and external consumers (e.g. agent-doc's live-document
//! model) can depend on it **without** a hard dependency on tsift. `tree-sitter-md`
//! is incremental and error-tolerant, suitable for a CRDT-backed live document.
//!
//! MemGraphRag and the rest of tsift's graph stack stay out of this crate.

use serde::{Deserialize, Serialize};

/// A byte-range text replacement suitable for CRDT/live-editor edit events.
///
/// The byte offsets describe one edit from the old source into the new source:
/// `start_byte..old_end_byte` in the old source became
/// `start_byte..new_end_byte` in the new source. Line/column points are derived
/// from the old and new source snapshots before calling tree-sitter. Conversion
/// fails unless the unchanged old/new prefix and suffix bytes match.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct MdTextEdit {
    pub start_byte: usize,
    pub old_end_byte: usize,
    pub new_end_byte: usize,
}

impl MdTextEdit {
    /// Construct an edit from absolute old/new byte range endpoints.
    pub fn replace(start_byte: usize, old_end_byte: usize, new_end_byte: usize) -> Self {
        Self {
            start_byte,
            old_end_byte,
            new_end_byte,
        }
    }

    /// Convert this edit into the tree-sitter edit shape.
    pub fn to_input_edit(
        self,
        old_source: &[u8],
        new_source: &[u8],
    ) -> Option<tree_sitter::InputEdit> {
        if self.start_byte > self.old_end_byte
            || self.old_end_byte > old_source.len()
            || self.start_byte > self.new_end_byte
            || self.new_end_byte > new_source.len()
        {
            return None;
        }
        if old_source.get(..self.start_byte)? != new_source.get(..self.start_byte)? {
            return None;
        }
        if old_source.get(self.old_end_byte..)? != new_source.get(self.new_end_byte..)? {
            return None;
        }
        Some(tree_sitter::InputEdit {
            start_byte: self.start_byte,
            old_end_byte: self.old_end_byte,
            new_end_byte: self.new_end_byte,
            start_position: markdown_point_for_byte(old_source, self.start_byte)?,
            old_end_position: markdown_point_for_byte(old_source, self.old_end_byte)?,
            new_end_position: markdown_point_for_byte(new_source, self.new_end_byte)?,
        })
    }
}

/// A markdown symbol: a heading-anchored section, a fenced code block, or a list
/// item. Byte spans and zero-based line numbers map directly onto the source.
/// This mirrors the shape tsift's graph layer consumes, but is owned here so the
/// leaf crate carries no tsift types.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MdSymbol {
    pub name: String,
    /// `heading` | `code_block` | `list_item`
    pub kind: String,
    /// Zero-based start line.
    pub line: usize,
    /// Zero-based end line.
    pub end_line: usize,
    /// tree-sitter node kind (`atx_heading` | `fenced_code_block` | `list_item`).
    pub node_kind: String,
    pub start_byte: usize,
    pub end_byte: usize,
    pub body_start_byte: Option<usize>,
    pub body_end_byte: Option<usize>,
}

/// The `tree-sitter-md` language used for markdown parsing.
pub fn markdown_language() -> tree_sitter::Language {
    tree_sitter_md::LANGUAGE.into()
}

/// Parse markdown source into a tree-sitter tree.
pub fn parse(source: &[u8]) -> Option<tree_sitter::Tree> {
    parse_with_old_tree(source, None)
}

/// Incrementally reparse markdown after a text edit.
///
/// This clones and edits `previous_tree`, so callers can keep their existing
/// tree snapshot if needed. Use [`reparse_incremental_with_input_edit`] when the
/// caller already has a tree-sitter `InputEdit`.
pub fn reparse_incremental(
    previous_tree: &tree_sitter::Tree,
    old_source: &[u8],
    new_source: &[u8],
    edit: MdTextEdit,
) -> Option<tree_sitter::Tree> {
    let input_edit = edit.to_input_edit(old_source, new_source)?;
    reparse_incremental_with_input_edit(previous_tree, new_source, &input_edit)
}

/// Incrementally reparse markdown with a precomputed tree-sitter edit.
pub fn reparse_incremental_with_input_edit(
    previous_tree: &tree_sitter::Tree,
    new_source: &[u8],
    edit: &tree_sitter::InputEdit,
) -> Option<tree_sitter::Tree> {
    let mut edited_tree = previous_tree.clone();
    edited_tree.edit(edit);
    parse_with_old_tree(new_source, Some(&edited_tree))
}

fn parse_with_old_tree(
    source: &[u8],
    old_tree: Option<&tree_sitter::Tree>,
) -> Option<tree_sitter::Tree> {
    let mut parser = tree_sitter::Parser::new();
    parser.set_language(&markdown_language()).ok()?;
    parser.parse(source, old_tree)
}

/// Parse `source` and extract its markdown symbols. Convenience for standalone
/// consumers; tsift's graph layer reuses an already-parsed tree via
/// [`markdown_symbols_from_tree`].
pub fn markdown_symbols(source: &[u8]) -> Vec<MdSymbol> {
    match parse(source) {
        Some(tree) => markdown_symbols_from_tree(&tree, source),
        None => Vec::new(),
    }
}

/// Extract markdown symbols from a previously-parsed tree. Headings become
/// sections that extend to the next heading of equal-or-shallower level.
pub fn markdown_symbols_from_tree(tree: &tree_sitter::Tree, source: &[u8]) -> Vec<MdSymbol> {
    let mut headings = Vec::new();
    let mut symbols = Vec::new();
    collect_markdown_symbols(tree.root_node(), source, &mut headings, &mut symbols);
    headings.sort_by(|left, right| {
        left.start_byte
            .cmp(&right.start_byte)
            .then(left.level.cmp(&right.level))
            .then(left.name.cmp(&right.name))
    });

    for (idx, heading) in headings.iter().enumerate() {
        let section_end_byte = headings
            .iter()
            .skip(idx + 1)
            .find(|candidate| candidate.level <= heading.level)
            .map(|candidate| candidate.start_byte)
            .unwrap_or(source.len());
        let body_start_byte =
            markdown_next_line_start(source, heading.heading_end_byte).min(section_end_byte);
        symbols.push(MdSymbol {
            name: heading.name.clone(),
            kind: "heading".to_string(),
            line: heading.start_line,
            end_line: markdown_zero_based_end_line(source, section_end_byte),
            node_kind: "atx_heading".to_string(),
            start_byte: heading.start_byte,
            end_byte: section_end_byte,
            body_start_byte: Some(body_start_byte),
            body_end_byte: Some(section_end_byte),
        });
    }

    symbols.sort_by(|left, right| {
        left.line
            .cmp(&right.line)
            .then(left.start_byte.cmp(&right.start_byte))
            .then(left.kind.cmp(&right.kind))
            .then(left.name.cmp(&right.name))
    });
    symbols
}

#[derive(Debug, Clone)]
struct MarkdownHeading {
    name: String,
    level: usize,
    start_byte: usize,
    heading_end_byte: usize,
    start_line: usize,
}

fn collect_markdown_symbols(
    node: tree_sitter::Node<'_>,
    source: &[u8],
    headings: &mut Vec<MarkdownHeading>,
    symbols: &mut Vec<MdSymbol>,
) {
    match node.kind() {
        "atx_heading" => {
            if let Some(level) = markdown_heading_level(node)
                && let Some(name) = markdown_heading_name(node, source)
            {
                headings.push(MarkdownHeading {
                    name,
                    level,
                    start_byte: node.start_byte(),
                    heading_end_byte: node.end_byte(),
                    start_line: node.start_position().row,
                });
            }
        }
        "fenced_code_block" => {
            let language = markdown_fenced_code_language(node, source)
                .filter(|value| !value.is_empty())
                .unwrap_or_else(|| "code".to_string());
            let body_span = markdown_fenced_code_body_span(node, source);
            symbols.push(MdSymbol {
                name: language,
                kind: "code_block".to_string(),
                line: node.start_position().row,
                end_line: markdown_zero_based_end_line(source, node.end_byte()),
                node_kind: "fenced_code_block".to_string(),
                start_byte: node.start_byte(),
                end_byte: node.end_byte(),
                body_start_byte: body_span.map(|(start, _)| start),
                body_end_byte: body_span.map(|(_, end)| end),
            });
        }
        "list_item" => {
            let name = markdown_list_item_name(node, source);
            symbols.push(MdSymbol {
                name,
                kind: "list_item".to_string(),
                line: node.start_position().row,
                end_line: markdown_zero_based_end_line(source, node.end_byte()),
                node_kind: "list_item".to_string(),
                start_byte: node.start_byte(),
                end_byte: node.end_byte(),
                body_start_byte: Some(node.start_byte()),
                body_end_byte: Some(node.end_byte()),
            });
        }
        _ => {}
    }

    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        collect_markdown_symbols(child, source, headings, symbols);
    }
}

fn markdown_heading_level(node: tree_sitter::Node<'_>) -> Option<usize> {
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        let kind = child.kind();
        if let Some(level) = kind
            .strip_prefix("atx_h")
            .and_then(|suffix| suffix.strip_suffix("_marker"))
            .and_then(|value| value.parse::<usize>().ok())
        {
            return Some(level);
        }
    }
    None
}

fn markdown_heading_name(node: tree_sitter::Node<'_>, source: &[u8]) -> Option<String> {
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if child.kind() == "inline" {
            let text = child.utf8_text(source).ok()?.trim();
            if !text.is_empty() {
                return Some(text.to_string());
            }
        }
    }
    let line = node.utf8_text(source).ok()?.lines().next()?.trim();
    let text = line.trim_start_matches('#').trim();
    (!text.is_empty()).then(|| text.to_string())
}

fn markdown_fenced_code_language(node: tree_sitter::Node<'_>, source: &[u8]) -> Option<String> {
    if node.kind() == "language" || node.kind() == "info_string" {
        let text = node.utf8_text(source).ok()?.trim();
        if !text.is_empty() {
            return Some(text.to_string());
        }
    }
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if let Some(language) = markdown_fenced_code_language(child, source) {
            return Some(language);
        }
    }
    None
}

fn markdown_fenced_code_body_span(
    node: tree_sitter::Node<'_>,
    source: &[u8],
) -> Option<(usize, usize)> {
    let text = node.utf8_text(source).ok()?;
    let first_newline = text.find('\n')?;
    let body_start = node.start_byte().saturating_add(first_newline + 1);
    let closing_start = source[node.start_byte()..node.end_byte()]
        .iter()
        .rposition(|byte| *byte == b'\n')
        .map(|offset| node.start_byte() + offset + 1)
        .unwrap_or(node.end_byte());
    Some((body_start.min(closing_start), closing_start))
}

fn markdown_list_item_name(node: tree_sitter::Node<'_>, source: &[u8]) -> String {
    let text = node.utf8_text(source).unwrap_or("");
    let first_line = text.lines().next().unwrap_or("").trim();
    let marker_stripped = first_line
        .strip_prefix("- ")
        .or_else(|| first_line.strip_prefix("* "))
        .or_else(|| first_line.strip_prefix("+ "))
        .or_else(|| {
            let (digits, rest) = first_line.split_at(
                first_line
                    .find(|ch: char| !ch.is_ascii_digit())
                    .unwrap_or(first_line.len()),
            );
            (!digits.is_empty())
                .then_some(rest)
                .and_then(|rest| rest.strip_prefix(". "))
        })
        .unwrap_or(first_line)
        .trim();
    if marker_stripped.is_empty() {
        "list item".to_string()
    } else {
        marker_stripped.chars().take(96).collect()
    }
}

fn markdown_next_line_start(source: &[u8], byte: usize) -> usize {
    let byte = byte.min(source.len());
    source[byte..]
        .iter()
        .position(|value| *value == b'\n')
        .map(|offset| byte + offset + 1)
        .unwrap_or(byte)
}

fn markdown_zero_based_end_line(source: &[u8], end_byte: usize) -> usize {
    let byte = end_byte.saturating_sub(1).min(source.len());
    source[..byte]
        .iter()
        .filter(|value| **value == b'\n')
        .count()
}

fn markdown_point_for_byte(source: &[u8], byte: usize) -> Option<tree_sitter::Point> {
    if byte > source.len() {
        return None;
    }
    let mut row = 0;
    let mut line_start = 0;
    for (idx, value) in source.iter().enumerate().take(byte) {
        if *value == b'\n' {
            row += 1;
            line_start = idx + 1;
        }
    }
    Some(tree_sitter::Point::new(row, byte - line_start))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_headings_into_sections() {
        let src = b"# Title\n\nintro\n\n## Section A\n\nbody a\n\n## Section B\n\nbody b\n";
        let symbols = markdown_symbols(src);
        let headings: Vec<_> = symbols.iter().filter(|s| s.kind == "heading").collect();
        assert_eq!(headings.len(), 3);
        let title = headings.iter().find(|s| s.name == "Title").unwrap();
        // Title is an h1; its section spans to EOF (no equal-or-shallower heading follows).
        let section_a = headings.iter().find(|s| s.name == "Section A").unwrap();
        let section_b = headings.iter().find(|s| s.name == "Section B").unwrap();
        // Section A ends where Section B (same level) begins.
        assert!(section_a.end_byte <= section_b.start_byte);
        assert!(title.start_byte < section_a.start_byte);
    }

    #[test]
    fn extracts_fenced_code_blocks_and_list_items() {
        let src = b"# Doc\n\n```rust\nfn main() {}\n```\n\n- first\n- second\n";
        let symbols = markdown_symbols(src);
        let code = symbols.iter().find(|s| s.kind == "code_block").unwrap();
        assert_eq!(code.name, "rust");
        assert!(code.body_start_byte.is_some());
        let items: Vec<_> = symbols.iter().filter(|s| s.kind == "list_item").collect();
        assert!(items.iter().any(|s| s.name == "first"));
        assert!(items.iter().any(|s| s.name == "second"));
    }

    #[test]
    fn empty_source_yields_no_symbols() {
        assert!(markdown_symbols(b"").is_empty());
    }

    #[test]
    fn from_tree_matches_convenience() {
        let src = b"# A\n\nx\n\n# B\n\ny\n";
        let tree = parse(src).unwrap();
        assert_eq!(
            markdown_symbols_from_tree(&tree, src),
            markdown_symbols(src)
        );
    }

    #[test]
    fn incremental_reparse_matches_full_parse_after_insert() {
        let old_src = b"# A\n\nbody\n";
        let new_src = b"# A\n\n## B\n\nbody\n";
        let old_tree = parse(old_src).unwrap();
        let edit = MdTextEdit::replace(5, 5, 11);

        let incremental_tree = reparse_incremental(&old_tree, old_src, new_src, edit).unwrap();
        let full_tree = parse(new_src).unwrap();

        assert_eq!(
            incremental_tree.root_node().to_sexp(),
            full_tree.root_node().to_sexp()
        );
        assert_eq!(
            markdown_symbols_from_tree(&incremental_tree, new_src),
            markdown_symbols(new_src)
        );
        assert!(
            markdown_symbols_from_tree(&old_tree, old_src)
                .iter()
                .all(|symbol| symbol.name != "B")
        );
    }

    #[test]
    fn incremental_reparse_rejects_mismatched_edit_sources() {
        let old_src = b"# A\n";
        let new_src = b"# B\n";
        let old_tree = parse(old_src).unwrap();
        let edit = MdTextEdit::replace(0, 0, 0);

        assert!(reparse_incremental(&old_tree, old_src, new_src, edit).is_none());
    }
}
