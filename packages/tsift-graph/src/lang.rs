use anyhow::Result;
use tree_sitter::{Language, Parser, Query, QueryCursor, StreamingIterator};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Symbol {
    pub name: String,
    pub kind: String,
    pub line: usize,
    pub end_line: usize,
    pub node_kind: String,
    pub start_byte: usize,
    pub end_byte: usize,
    pub body_start_byte: Option<usize>,
    pub body_end_byte: Option<usize>,
}

#[allow(dead_code)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Lang {
    #[cfg(feature = "lang-rust")]
    Rust,
    #[cfg(feature = "lang-python")]
    Python,
    #[cfg(feature = "lang-typescript")]
    TypeScript,
    #[cfg(feature = "lang-typescript")]
    Tsx,
    #[cfg(feature = "lang-javascript")]
    JavaScript,
    #[cfg(feature = "lang-javascript")]
    Jsx,
    #[cfg(feature = "lang-kotlin")]
    Kotlin,
    #[cfg(feature = "lang-zig")]
    Zig,
    #[cfg(feature = "lang-bash")]
    Bash,
    #[cfg(feature = "lang-markdown")]
    Markdown,
}

#[allow(dead_code)]
impl Lang {
    pub fn from_extension(ext: &str) -> Option<Self> {
        match ext {
            #[cfg(feature = "lang-rust")]
            "rs" => Some(Self::Rust),
            #[cfg(feature = "lang-python")]
            "py" | "pyi" => Some(Self::Python),
            #[cfg(feature = "lang-typescript")]
            "ts" => Some(Self::TypeScript),
            #[cfg(feature = "lang-typescript")]
            "tsx" => Some(Self::Tsx),
            #[cfg(feature = "lang-javascript")]
            "js" | "mjs" | "cjs" => Some(Self::JavaScript),
            #[cfg(feature = "lang-javascript")]
            "jsx" => Some(Self::Jsx),
            #[cfg(feature = "lang-kotlin")]
            "kt" | "kts" => Some(Self::Kotlin),
            #[cfg(feature = "lang-zig")]
            "zig" => Some(Self::Zig),
            #[cfg(feature = "lang-bash")]
            "sh" | "bash" | "zsh" => Some(Self::Bash),
            #[cfg(feature = "lang-markdown")]
            "md" | "mdx" => Some(Self::Markdown),
            _ => None,
        }
    }

    pub fn tree_sitter_language(&self) -> Language {
        match self {
            #[cfg(feature = "lang-rust")]
            Self::Rust => tree_sitter_rust::LANGUAGE.into(),
            #[cfg(feature = "lang-python")]
            Self::Python => tree_sitter_python::LANGUAGE.into(),
            #[cfg(feature = "lang-typescript")]
            Self::TypeScript => tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into(),
            #[cfg(feature = "lang-typescript")]
            Self::Tsx => tree_sitter_typescript::LANGUAGE_TSX.into(),
            #[cfg(feature = "lang-javascript")]
            Self::JavaScript => tree_sitter_javascript::LANGUAGE.into(),
            #[cfg(feature = "lang-javascript")]
            Self::Jsx => tree_sitter_javascript::LANGUAGE.into(),
            #[cfg(feature = "lang-kotlin")]
            Self::Kotlin => tree_sitter_kotlin_ng::LANGUAGE.into(),
            #[cfg(feature = "lang-zig")]
            Self::Zig => tree_sitter_zig::LANGUAGE.into(),
            #[cfg(feature = "lang-bash")]
            Self::Bash => tree_sitter_bash::LANGUAGE.into(),
            #[cfg(feature = "lang-markdown")]
            Self::Markdown => tree_sitter_md::LANGUAGE.into(),
        }
    }

    pub fn name(&self) -> &'static str {
        match self {
            #[cfg(feature = "lang-rust")]
            Self::Rust => "rust",
            #[cfg(feature = "lang-python")]
            Self::Python => "python",
            #[cfg(feature = "lang-typescript")]
            Self::TypeScript => "typescript",
            #[cfg(feature = "lang-typescript")]
            Self::Tsx => "tsx",
            #[cfg(feature = "lang-javascript")]
            Self::JavaScript => "javascript",
            #[cfg(feature = "lang-javascript")]
            Self::Jsx => "jsx",
            #[cfg(feature = "lang-kotlin")]
            Self::Kotlin => "kotlin",
            #[cfg(feature = "lang-zig")]
            Self::Zig => "zig",
            #[cfg(feature = "lang-bash")]
            Self::Bash => "bash",
            #[cfg(feature = "lang-markdown")]
            Self::Markdown => "markdown",
        }
    }

    pub fn symbol_query(&self) -> &'static str {
        match self {
            #[cfg(feature = "lang-rust")]
            Self::Rust => {
                r#"
                (function_item name: (identifier) @function.name)
                (struct_item name: (type_identifier) @struct.name)
                (enum_item name: (type_identifier) @enum.name)
                (trait_item name: (type_identifier) @trait.name)
                (impl_item type: (type_identifier) @impl.name)
                (mod_item name: (identifier) @mod.name)
                (type_item name: (type_identifier) @type_alias.name)
                (const_item name: (identifier) @const.name)
                (static_item name: (identifier) @static.name)
            "#
            }
            #[cfg(feature = "lang-python")]
            Self::Python => {
                r#"
                (function_definition name: (identifier) @function.name)
                (class_definition name: (identifier) @class.name)
            "#
            }
            #[cfg(feature = "lang-typescript")]
            Self::TypeScript | Self::Tsx => {
                r#"
                (function_declaration name: (identifier) @function.name)
                (class_declaration name: (type_identifier) @class.name)
                (interface_declaration name: (type_identifier) @interface.name)
                (type_alias_declaration name: (type_identifier) @type_alias.name)
                (enum_declaration name: (identifier) @enum.name)
                (variable_declarator name: (identifier) @function.name value: (arrow_function))
            "#
            }
            #[cfg(feature = "lang-javascript")]
            Self::JavaScript | Self::Jsx => {
                r#"
                (function_declaration name: (identifier) @function.name)
                (class_declaration name: (identifier) @class.name)
                (variable_declarator name: (identifier) @function.name value: (arrow_function))
            "#
            }
            #[cfg(feature = "lang-kotlin")]
            Self::Kotlin => {
                r#"
                (function_declaration name: (identifier) @function.name)
                (class_declaration "interface" name: (identifier) @interface.name)
                (class_declaration (modifiers (class_modifier "data")) name: (identifier) @data_class.name)
                (class_declaration (modifiers (class_modifier "sealed")) name: (identifier) @sealed_class.name)
                (class_declaration (modifiers (class_modifier "enum")) name: (identifier) @enum_class.name)
                (class_declaration "class" name: (identifier) @class.name)
                (object_declaration name: (identifier) @object.name)
                (companion_object name: (identifier) @companion_object.name)
            "#
            }
            #[cfg(feature = "lang-zig")]
            Self::Zig => {
                r#"
                (function_declaration (identifier) @function.name)
                (variable_declaration (identifier) @struct.name (struct_declaration))
                (variable_declaration (identifier) @enum.name (enum_declaration))
                (variable_declaration (identifier) @union.name (union_declaration))
                (variable_declaration (identifier) @const.name)
            "#
            }
            #[cfg(feature = "lang-bash")]
            Self::Bash => {
                r#"
                (function_definition name: (word) @function.name)
            "#
            }
            #[cfg(feature = "lang-markdown")]
            Self::Markdown => {
                r#"
                (atx_heading (atx_h1_marker) (inline) @heading.name)
                (atx_heading (atx_h2_marker) (inline) @heading.name)
                (atx_heading (atx_h3_marker) (inline) @heading.name)
                (atx_heading (atx_h4_marker) (inline) @heading.name)
                (atx_heading (atx_h5_marker) (inline) @heading.name)
                (atx_heading (atx_h6_marker) (inline) @heading.name)
                (fenced_code_block (info_string (language) @code_block.name))
            "#
            }
        }
    }

    pub fn call_query(&self) -> Option<&'static str> {
        match self {
            #[cfg(feature = "lang-rust")]
            Self::Rust => Some(
                r#"
                (call_expression function: (identifier) @call.name)
                (call_expression function: (field_expression field: (field_identifier) @call.name))
                (call_expression function: (scoped_identifier name: (identifier) @call.name))
                (macro_invocation macro: (identifier) @call.name)
            "#,
            ),
            #[cfg(feature = "lang-python")]
            Self::Python => Some(
                r#"
                (call function: (identifier) @call.name)
                (call function: (attribute attribute: (identifier) @call.name))
            "#,
            ),
            #[cfg(feature = "lang-typescript")]
            Self::TypeScript | Self::Tsx => Some(
                r#"
                (call_expression function: (identifier) @call.name)
                (call_expression function: (member_expression property: (property_identifier) @call.name))
            "#,
            ),
            #[cfg(feature = "lang-javascript")]
            Self::JavaScript | Self::Jsx => Some(
                r#"
                (call_expression function: (identifier) @call.name)
                (call_expression function: (member_expression property: (property_identifier) @call.name))
            "#,
            ),
            #[cfg(feature = "lang-kotlin")]
            Self::Kotlin => Some(
                r#"
                (call_expression (simple_identifier) @call.name)
            "#,
            ),
            _ => None,
        }
    }

    pub fn extract_symbols(&self, source: &[u8]) -> Result<Vec<Symbol>> {
        let mut parser = Parser::new();
        let ts_lang = self.tree_sitter_language();
        parser.set_language(&ts_lang)?;
        let tree = parser
            .parse(source, None)
            .ok_or_else(|| anyhow::anyhow!("parse failed"))?;
        #[cfg(feature = "lang-markdown")]
        if *self == Self::Markdown {
            return Ok(extract_markdown_symbols(&tree, source));
        }
        let query = Query::new(&ts_lang, self.symbol_query())?;
        let mut cursor = QueryCursor::new();
        let mut symbols = Vec::new();
        let capture_names: Vec<String> = query
            .capture_names()
            .iter()
            .map(|s| s.to_string())
            .collect();

        let mut matches = cursor.matches(&query, tree.root_node(), source);
        while let Some(m) = matches.next() {
            for capture in m.captures {
                let capture_name = &capture_names[capture.index as usize];
                if let Some(kind_str) = capture_name.strip_suffix(".name") {
                    let name = capture
                        .node
                        .utf8_text(source)
                        .unwrap_or("<invalid utf8>")
                        .to_string();
                    let node = symbol_node_for_capture(kind_str, capture.node);
                    let body_span = symbol_body_span(node);
                    symbols.push(Symbol {
                        name,
                        kind: kind_str.to_string(),
                        line: node.start_position().row,
                        end_line: node.end_position().row,
                        node_kind: node.kind().to_string(),
                        start_byte: node.start_byte(),
                        end_byte: node.end_byte(),
                        body_start_byte: body_span.map(|(start, _)| start),
                        body_end_byte: body_span.map(|(_, end)| end),
                    });
                }
            }
        }

        #[cfg(feature = "lang-bash")]
        if *self == Self::Bash {
            Self::extract_bash_aliases(&tree, source, &mut symbols);
        }
        symbols.sort_by(|a, b| a.line.cmp(&b.line).then(a.name.cmp(&b.name)));
        symbols.dedup_by(|b, a| {
            a.name == b.name && a.line == b.line && {
                let a_generic = matches!(a.kind.as_str(), "variable" | "const");
                let b_generic = matches!(b.kind.as_str(), "variable" | "const");
                match (a_generic, b_generic) {
                    (true, false) => a.kind.clone_from(&b.kind),
                    (false, true) => {}
                    _ => {
                        if b.kind.len() > a.kind.len() {
                            a.kind.clone_from(&b.kind);
                        }
                    }
                }
                true
            }
        });
        Ok(symbols)
    }

    #[cfg(feature = "lang-bash")]
    fn extract_bash_aliases(tree: &tree_sitter::Tree, source: &[u8], symbols: &mut Vec<Symbol>) {
        let mut tree_cursor = tree.root_node().walk();
        if !tree_cursor.goto_first_child() {
            return;
        }
        loop {
            let node = tree_cursor.node();
            if node.kind() == "command"
                && let Some(name_node) = node.child_by_field_name("name")
            {
                let cmd = name_node.utf8_text(source).unwrap_or("");
                if cmd == "alias" {
                    for i in 0..node.named_child_count() {
                        if let Some(arg) = node.named_child(i as u32)
                            && (arg.kind() == "concatenation" || arg.kind() == "word")
                        {
                            let text = arg.utf8_text(source).unwrap_or("");
                            if let Some(alias_name) = text.split('=').next()
                                && !alias_name.is_empty()
                                && alias_name != cmd
                            {
                                symbols.push(Symbol {
                                    name: alias_name.to_string(),
                                    kind: "alias".to_string(),
                                    line: arg.start_position().row,
                                    end_line: node.end_position().row,
                                    node_kind: node.kind().to_string(),
                                    start_byte: arg.start_byte(),
                                    end_byte: node.end_byte(),
                                    body_start_byte: None,
                                    body_end_byte: None,
                                });
                            }
                        }
                    }
                }
            }
            if !tree_cursor.goto_next_sibling() {
                break;
            }
        }
    }

    pub fn all() -> Vec<Self> {
        vec![
            #[cfg(feature = "lang-rust")]
            Self::Rust,
            #[cfg(feature = "lang-python")]
            Self::Python,
            #[cfg(feature = "lang-typescript")]
            Self::TypeScript,
            #[cfg(feature = "lang-typescript")]
            Self::Tsx,
            #[cfg(feature = "lang-javascript")]
            Self::JavaScript,
            #[cfg(feature = "lang-javascript")]
            Self::Jsx,
            #[cfg(feature = "lang-kotlin")]
            Self::Kotlin,
            #[cfg(feature = "lang-zig")]
            Self::Zig,
            #[cfg(feature = "lang-bash")]
            Self::Bash,
            #[cfg(feature = "lang-markdown")]
            Self::Markdown,
        ]
    }
}

fn symbol_node_for_capture<'tree>(
    kind: &str,
    name_node: tree_sitter::Node<'tree>,
) -> tree_sitter::Node<'tree> {
    let mut node = name_node.parent().unwrap_or(name_node);
    if kind == "code_block" {
        while let Some(parent) = node.parent() {
            node = parent;
            if node.kind() == "fenced_code_block" {
                break;
            }
        }
    }
    node
}

fn symbol_body_span(node: tree_sitter::Node<'_>) -> Option<(usize, usize)> {
    if let Some(body) = node.child_by_field_name("body") {
        return Some((body.start_byte(), body.end_byte()));
    }
    for idx in 0..node.named_child_count() {
        let Some(child) = node.named_child(idx as u32) else {
            continue;
        };
        if matches!(
            child.kind(),
            "block"
                | "declaration_list"
                | "field_declaration_list"
                | "enum_variant_list"
                | "match_block"
                | "statement_block"
                | "suite"
        ) {
            return Some((child.start_byte(), child.end_byte()));
        }
    }
    None
}

#[cfg(feature = "lang-markdown")]
#[derive(Debug, Clone)]
struct MarkdownHeading {
    name: String,
    level: usize,
    start_byte: usize,
    heading_end_byte: usize,
    start_line: usize,
}

#[cfg(feature = "lang-markdown")]
fn extract_markdown_symbols(tree: &tree_sitter::Tree, source: &[u8]) -> Vec<Symbol> {
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
        symbols.push(Symbol {
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

#[cfg(feature = "lang-markdown")]
fn collect_markdown_symbols(
    node: tree_sitter::Node<'_>,
    source: &[u8],
    headings: &mut Vec<MarkdownHeading>,
    symbols: &mut Vec<Symbol>,
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
            symbols.push(Symbol {
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
            symbols.push(Symbol {
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

#[cfg(feature = "lang-markdown")]
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

#[cfg(feature = "lang-markdown")]
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

#[cfg(feature = "lang-markdown")]
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

#[cfg(feature = "lang-markdown")]
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

#[cfg(feature = "lang-markdown")]
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

#[cfg(feature = "lang-markdown")]
fn markdown_next_line_start(source: &[u8], byte: usize) -> usize {
    let byte = byte.min(source.len());
    source[byte..]
        .iter()
        .position(|value| *value == b'\n')
        .map(|offset| byte + offset + 1)
        .unwrap_or(byte)
}

#[cfg(feature = "lang-markdown")]
fn markdown_zero_based_end_line(source: &[u8], end_byte: usize) -> usize {
    let byte = end_byte.saturating_sub(1).min(source.len());
    source[..byte]
        .iter()
        .filter(|value| **value == b'\n')
        .count()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_all_grammars_create_parser() {
        for lang in Lang::all() {
            let ts_lang = lang.tree_sitter_language();
            let mut parser = tree_sitter::Parser::new();
            parser
                .set_language(&ts_lang)
                .unwrap_or_else(|e| panic!("failed to set language for {:?}: {}", lang, e));
        }
    }

    #[test]
    fn test_extension_dispatch() {
        let cases = [
            ("rs", "rust"),
            ("py", "python"),
            ("pyi", "python"),
            ("ts", "typescript"),
            ("tsx", "tsx"),
            ("js", "javascript"),
            ("mjs", "javascript"),
            ("cjs", "javascript"),
            ("jsx", "jsx"),
            ("kt", "kotlin"),
            ("kts", "kotlin"),
            ("zig", "zig"),
            ("sh", "bash"),
            ("bash", "bash"),
            ("zsh", "bash"),
            ("md", "markdown"),
            ("mdx", "markdown"),
        ];
        for (ext, expected_name) in cases {
            let lang = Lang::from_extension(ext)
                .unwrap_or_else(|| panic!("no language for extension: {ext}"));
            assert_eq!(lang.name(), expected_name, "wrong language for .{ext}");
        }
    }

    #[test]
    fn test_unknown_extension_returns_none() {
        assert!(Lang::from_extension("xyz").is_none());
        assert!(Lang::from_extension("").is_none());
        assert!(Lang::from_extension("txt").is_none());
    }

    #[cfg(feature = "lang-rust")]
    #[test]
    fn test_parse_rust_snippet() {
        let lang = Lang::Rust;
        let mut parser = tree_sitter::Parser::new();
        parser.set_language(&lang.tree_sitter_language()).unwrap();
        let tree = parser.parse("fn main() {}", None).unwrap();
        assert_eq!(tree.root_node().kind(), "source_file");
        assert!(!tree.root_node().has_error());
    }

    #[cfg(feature = "lang-python")]
    #[test]
    fn test_parse_python_snippet() {
        let lang = Lang::Python;
        let mut parser = tree_sitter::Parser::new();
        parser.set_language(&lang.tree_sitter_language()).unwrap();
        let tree = parser.parse("def hello():\n    pass\n", None).unwrap();
        assert_eq!(tree.root_node().kind(), "module");
        assert!(!tree.root_node().has_error());
    }

    #[cfg(feature = "lang-typescript")]
    #[test]
    fn test_parse_typescript_snippet() {
        let lang = Lang::TypeScript;
        let mut parser = tree_sitter::Parser::new();
        parser.set_language(&lang.tree_sitter_language()).unwrap();
        let tree = parser
            .parse("function greet(name: string): void {}", None)
            .unwrap();
        assert_eq!(tree.root_node().kind(), "program");
        assert!(!tree.root_node().has_error());
    }

    #[cfg(feature = "lang-typescript")]
    #[test]
    fn test_parse_tsx_snippet() {
        let lang = Lang::Tsx;
        let mut parser = tree_sitter::Parser::new();
        parser.set_language(&lang.tree_sitter_language()).unwrap();
        let tree = parser
            .parse("const App = () => <div>hello</div>;", None)
            .unwrap();
        assert_eq!(tree.root_node().kind(), "program");
        assert!(!tree.root_node().has_error());
    }

    #[cfg(feature = "lang-javascript")]
    #[test]
    fn test_parse_javascript_snippet() {
        let lang = Lang::JavaScript;
        let mut parser = tree_sitter::Parser::new();
        parser.set_language(&lang.tree_sitter_language()).unwrap();
        let tree = parser
            .parse("function hello() { return 42; }", None)
            .unwrap();
        assert_eq!(tree.root_node().kind(), "program");
        assert!(!tree.root_node().has_error());
    }

    #[cfg(feature = "lang-kotlin")]
    #[test]
    fn test_parse_kotlin_snippet() {
        let lang = Lang::Kotlin;
        let mut parser = tree_sitter::Parser::new();
        parser.set_language(&lang.tree_sitter_language()).unwrap();
        let tree = parser
            .parse("fun main() { println(\"hello\") }", None)
            .unwrap();
        assert_eq!(tree.root_node().kind(), "source_file");
        assert!(!tree.root_node().has_error());
    }

    #[cfg(feature = "lang-zig")]
    #[test]
    fn test_parse_zig_snippet() {
        let lang = Lang::Zig;
        let mut parser = tree_sitter::Parser::new();
        parser.set_language(&lang.tree_sitter_language()).unwrap();
        let tree = parser.parse("pub fn main() !void {}", None).unwrap();
        assert_eq!(tree.root_node().kind(), "source_file");
    }

    #[cfg(feature = "lang-bash")]
    #[test]
    fn test_parse_bash_snippet() {
        let lang = Lang::Bash;
        let mut parser = tree_sitter::Parser::new();
        parser.set_language(&lang.tree_sitter_language()).unwrap();
        let tree = parser
            .parse("#!/bin/bash\nhello() { echo hi; }\n", None)
            .unwrap();
        assert_eq!(tree.root_node().kind(), "program");
        assert!(!tree.root_node().has_error());
    }

    #[cfg(feature = "lang-markdown")]
    #[test]
    fn test_parse_markdown_snippet() {
        let lang = Lang::Markdown;
        let mut parser = tree_sitter::Parser::new();
        parser.set_language(&lang.tree_sitter_language()).unwrap();
        let tree = parser.parse("# Hello\n\nSome text.\n", None).unwrap();
        assert_eq!(tree.root_node().kind(), "document");
        assert!(!tree.root_node().has_error());
    }

    #[test]
    fn test_all_symbol_queries_compile() {
        for lang in Lang::all() {
            let ts_lang = lang.tree_sitter_language();
            tree_sitter::Query::new(&ts_lang, lang.symbol_query())
                .unwrap_or_else(|e| panic!("query compile failed for {:?}: {}", lang, e));
        }
    }

    #[cfg(feature = "lang-rust")]
    #[test]
    fn test_extract_rust_symbols() {
        let source = b"fn main() {}\nstruct Foo;\nenum Bar {}\ntrait Baz {}\nconst X: i32 = 1;\nstatic Y: i32 = 2;\nmod inner {}\ntype Alias = i32;\n";
        let symbols = Lang::Rust.extract_symbols(source).unwrap();
        let names: Vec<&str> = symbols.iter().map(|s| s.name.as_str()).collect();
        assert!(names.contains(&"main"), "missing main, got {:?}", names);
        assert!(names.contains(&"Foo"), "missing Foo, got {:?}", names);
        assert!(names.contains(&"Bar"), "missing Bar, got {:?}", names);
        assert!(names.contains(&"Baz"), "missing Baz, got {:?}", names);
        assert!(names.contains(&"X"), "missing X, got {:?}", names);
        assert!(names.contains(&"Y"), "missing Y, got {:?}", names);
        assert!(names.contains(&"inner"), "missing inner, got {:?}", names);
        assert!(names.contains(&"Alias"), "missing Alias, got {:?}", names);
        let main_sym = symbols.iter().find(|s| s.name == "main").unwrap();
        assert_eq!(main_sym.kind, "function");
        let foo_sym = symbols.iter().find(|s| s.name == "Foo").unwrap();
        assert_eq!(foo_sym.kind, "struct");
    }

    #[cfg(feature = "lang-python")]
    #[test]
    fn test_extract_python_symbols() {
        let source =
            b"def hello():\n    pass\n\nclass MyClass:\n    def method(self):\n        pass\n";
        let symbols = Lang::Python.extract_symbols(source).unwrap();
        let names: Vec<&str> = symbols.iter().map(|s| s.name.as_str()).collect();
        assert!(names.contains(&"hello"), "missing hello, got {:?}", names);
        assert!(
            names.contains(&"MyClass"),
            "missing MyClass, got {:?}",
            names
        );
        assert!(names.contains(&"method"), "missing method, got {:?}", names);
        let cls = symbols.iter().find(|s| s.name == "MyClass").unwrap();
        assert_eq!(cls.kind, "class");
    }

    #[cfg(feature = "lang-typescript")]
    #[test]
    fn test_extract_typescript_symbols() {
        let source = b"function greet(name: string): void {}\nclass Foo {}\ninterface Bar {}\ntype Alias = string;\nenum Color { Red, Green }\n";
        let symbols = Lang::TypeScript.extract_symbols(source).unwrap();
        let names: Vec<&str> = symbols.iter().map(|s| s.name.as_str()).collect();
        assert!(names.contains(&"greet"), "missing greet, got {:?}", names);
        assert!(names.contains(&"Foo"), "missing Foo, got {:?}", names);
        assert!(names.contains(&"Bar"), "missing Bar, got {:?}", names);
        assert!(names.contains(&"Alias"), "missing Alias, got {:?}", names);
        assert!(names.contains(&"Color"), "missing Color, got {:?}", names);
    }

    #[cfg(feature = "lang-javascript")]
    #[test]
    fn test_extract_javascript_symbols() {
        let source = b"function hello() { return 42; }\nclass Widget {}\n";
        let symbols = Lang::JavaScript.extract_symbols(source).unwrap();
        let names: Vec<&str> = symbols.iter().map(|s| s.name.as_str()).collect();
        assert!(names.contains(&"hello"), "missing hello, got {:?}", names);
        assert!(names.contains(&"Widget"), "missing Widget, got {:?}", names);
    }

    #[cfg(feature = "lang-kotlin")]
    #[test]
    fn test_extract_kotlin_symbols() {
        let source = b"fun main() { println(\"hi\") }\nclass Foo\ninterface Bar\ndata class Baz(val x: Int)\nsealed class Qux\nenum class Color { RED, GREEN }\nobject Singleton\n";
        let symbols = Lang::Kotlin.extract_symbols(source).unwrap();
        let names: Vec<&str> = symbols.iter().map(|s| s.name.as_str()).collect();
        assert!(names.contains(&"main"), "missing main, got {:?}", names);
        assert!(names.contains(&"Foo"), "missing Foo, got {:?}", names);
        assert!(names.contains(&"Bar"), "missing Bar, got {:?}", names);
        assert!(names.contains(&"Baz"), "missing Baz, got {:?}", names);
        assert!(names.contains(&"Qux"), "missing Qux, got {:?}", names);
        assert!(names.contains(&"Color"), "missing Color, got {:?}", names);
        assert!(
            names.contains(&"Singleton"),
            "missing Singleton, got {:?}",
            names
        );
        let main_sym = symbols.iter().find(|s| s.name == "main").unwrap();
        assert_eq!(main_sym.kind, "function");
        let foo_sym = symbols.iter().find(|s| s.name == "Foo").unwrap();
        assert_eq!(foo_sym.kind, "class");
        let bar_sym = symbols.iter().find(|s| s.name == "Bar").unwrap();
        assert_eq!(bar_sym.kind, "interface");
        let baz_sym = symbols.iter().find(|s| s.name == "Baz").unwrap();
        assert_eq!(baz_sym.kind, "data_class");
        let qux_sym = symbols.iter().find(|s| s.name == "Qux").unwrap();
        assert_eq!(qux_sym.kind, "sealed_class");
        let color_sym = symbols.iter().find(|s| s.name == "Color").unwrap();
        assert_eq!(color_sym.kind, "enum_class");
        let singleton_sym = symbols.iter().find(|s| s.name == "Singleton").unwrap();
        assert_eq!(singleton_sym.kind, "object");
        assert_eq!(
            symbols.len(),
            7,
            "expected exactly 7 symbols, got {:?}",
            symbols
        );
    }

    #[cfg(feature = "lang-zig")]
    #[test]
    fn test_extract_zig_symbols() {
        let source = b"const std = @import(\"std\");\npub fn main() !void {}\nconst Point = struct { x: i32, y: i32 };\nconst Color = enum { red, green, blue };\nconst Result = union(enum) { ok: i32, err: []const u8 };\nconst MAX: i32 = 100;\n";
        let symbols = Lang::Zig.extract_symbols(source).unwrap();
        let names: Vec<&str> = symbols.iter().map(|s| s.name.as_str()).collect();
        assert!(names.contains(&"main"), "missing main, got {:?}", names);
        assert!(names.contains(&"Point"), "missing Point, got {:?}", names);
        assert!(names.contains(&"Color"), "missing Color, got {:?}", names);
        assert!(names.contains(&"Result"), "missing Result, got {:?}", names);
        assert!(names.contains(&"std"), "missing std, got {:?}", names);
        assert!(names.contains(&"MAX"), "missing MAX, got {:?}", names);
        let main_sym = symbols.iter().find(|s| s.name == "main").unwrap();
        assert_eq!(main_sym.kind, "function");
        let point_sym = symbols.iter().find(|s| s.name == "Point").unwrap();
        assert_eq!(point_sym.kind, "struct");
        let color_sym = symbols.iter().find(|s| s.name == "Color").unwrap();
        assert_eq!(color_sym.kind, "enum");
        let result_sym = symbols.iter().find(|s| s.name == "Result").unwrap();
        assert_eq!(result_sym.kind, "union");
        let max_sym = symbols.iter().find(|s| s.name == "MAX").unwrap();
        assert_eq!(max_sym.kind, "const");
    }

    #[cfg(feature = "lang-bash")]
    #[test]
    fn test_extract_bash_symbols() {
        let source = b"#!/bin/bash\nhello() { echo hi; }\nfunction world { echo world; }\nalias ll='ls -la'\nalias grep='grep --color=auto'\n";
        let symbols = Lang::Bash.extract_symbols(source).unwrap();
        let names: Vec<&str> = symbols.iter().map(|s| s.name.as_str()).collect();
        assert!(names.contains(&"hello"), "missing hello, got {:?}", names);
        assert!(names.contains(&"world"), "missing world, got {:?}", names);
        assert!(names.contains(&"ll"), "missing alias ll, got {:?}", names);
        assert!(
            names.contains(&"grep"),
            "missing alias grep, got {:?}",
            names
        );
        let hello_sym = symbols.iter().find(|s| s.name == "hello").unwrap();
        assert_eq!(hello_sym.kind, "function");
        let ll_sym = symbols.iter().find(|s| s.name == "ll").unwrap();
        assert_eq!(ll_sym.kind, "alias");
    }

    #[cfg(feature = "lang-markdown")]
    #[test]
    fn test_extract_markdown_symbols() {
        let source = b"# Title\n\n## Section One\n\nSome text.\n\n- Run setup\n  - Confirm setup\n\n```rust\nfn main() {}\n```\n\n### Subsection\n\n```python\ndef hello():\n    pass\n```\n\n## Next Section\n\nDone.\n";
        let symbols = Lang::Markdown.extract_symbols(source).unwrap();
        let headings: Vec<&Symbol> = symbols.iter().filter(|s| s.kind == "heading").collect();
        let code_blocks: Vec<&Symbol> = symbols.iter().filter(|s| s.kind == "code_block").collect();
        let list_items: Vec<&Symbol> = symbols.iter().filter(|s| s.kind == "list_item").collect();
        assert_eq!(headings.len(), 4, "expected 4 headings, got {:?}", headings);
        assert_eq!(
            code_blocks.len(),
            2,
            "expected 2 code blocks, got {:?}",
            code_blocks
        );
        assert_eq!(
            list_items.len(),
            2,
            "expected 2 list items, got {:?}",
            list_items
        );
        let title = headings.iter().find(|s| s.name == "Title").unwrap();
        let section = headings.iter().find(|s| s.name == "Section One").unwrap();
        let next = headings.iter().find(|s| s.name == "Next Section").unwrap();
        assert_eq!(title.node_kind, "atx_heading");
        assert!(title.end_byte > next.start_byte);
        assert_eq!(section.end_byte, next.start_byte);
        assert!(
            section.body_start_byte.unwrap() > section.start_byte,
            "heading body should begin after the marker line"
        );
        assert!(
            code_blocks.iter().any(|s| s.name == "rust"),
            "missing rust block, got {:?}",
            code_blocks
        );
        assert!(
            code_blocks.iter().any(|s| s.name == "python"),
            "missing python block, got {:?}",
            code_blocks
        );
        assert!(
            list_items.iter().any(|s| s.name == "Run setup"),
            "missing top-level list item, got {:?}",
            list_items
        );
    }

    #[cfg(feature = "lang-python")]
    #[test]
    fn test_python_async_def() {
        let source = b"async def fetch_data():\n    await get()\n\ndef sync_fn():\n    pass\n";
        let symbols = Lang::Python.extract_symbols(source).unwrap();
        let names: Vec<&str> = symbols.iter().map(|s| s.name.as_str()).collect();
        assert!(
            names.contains(&"fetch_data"),
            "missing async function, got {:?}",
            names
        );
        assert!(
            names.contains(&"sync_fn"),
            "missing sync function, got {:?}",
            names
        );
    }

    #[cfg(feature = "lang-python")]
    #[test]
    fn test_python_decorated_function() {
        let source = b"@staticmethod\ndef helper():\n    pass\n\n@property\ndef name(self):\n    return self._name\n";
        let symbols = Lang::Python.extract_symbols(source).unwrap();
        let names: Vec<&str> = symbols.iter().map(|s| s.name.as_str()).collect();
        assert!(
            names.contains(&"helper"),
            "missing decorated function, got {:?}",
            names
        );
        assert!(
            names.contains(&"name"),
            "missing property function, got {:?}",
            names
        );
    }

    #[cfg(feature = "lang-typescript")]
    #[test]
    fn test_typescript_arrow_exports() {
        let source = b"export const Foo = () => { return 42; };\nexport const Bar = (x: number): number => x + 1;\nconst local = () => {};\nfunction regular() {}\n";
        let symbols = Lang::TypeScript.extract_symbols(source).unwrap();
        let names: Vec<&str> = symbols.iter().map(|s| s.name.as_str()).collect();
        assert!(
            names.contains(&"Foo"),
            "missing arrow export Foo, got {:?}",
            names
        );
        assert!(
            names.contains(&"Bar"),
            "missing arrow export Bar, got {:?}",
            names
        );
        assert!(
            names.contains(&"local"),
            "missing local arrow, got {:?}",
            names
        );
        assert!(
            names.contains(&"regular"),
            "missing regular function, got {:?}",
            names
        );
    }

    #[cfg(feature = "lang-typescript")]
    #[test]
    fn test_tsx_arrow_component() {
        let source = b"export const MyComponent = () => <div>hello</div>;\nfunction Other() { return <span/>; }\n";
        let symbols = Lang::Tsx.extract_symbols(source).unwrap();
        let names: Vec<&str> = symbols.iter().map(|s| s.name.as_str()).collect();
        assert!(
            names.contains(&"MyComponent"),
            "missing arrow component, got {:?}",
            names
        );
        assert!(
            names.contains(&"Other"),
            "missing function component, got {:?}",
            names
        );
    }

    #[cfg(feature = "lang-javascript")]
    #[test]
    fn test_javascript_arrow_exports() {
        let source = b"export const handler = () => { return 'ok'; };\nconst helper = (x) => x * 2;\nfunction regular() {}\n";
        let symbols = Lang::JavaScript.extract_symbols(source).unwrap();
        let names: Vec<&str> = symbols.iter().map(|s| s.name.as_str()).collect();
        assert!(
            names.contains(&"handler"),
            "missing arrow export, got {:?}",
            names
        );
        assert!(
            names.contains(&"helper"),
            "missing local arrow, got {:?}",
            names
        );
        assert!(
            names.contains(&"regular"),
            "missing regular function, got {:?}",
            names
        );
    }

    #[cfg(feature = "lang-javascript")]
    #[test]
    fn test_jsx_arrow_component() {
        let source = b"const App = () => <div>hi</div>;\nfunction Page() { return <main/>; }\n";
        let symbols = Lang::Jsx.extract_symbols(source).unwrap();
        let names: Vec<&str> = symbols.iter().map(|s| s.name.as_str()).collect();
        assert!(
            names.contains(&"App"),
            "missing arrow JSX component, got {:?}",
            names
        );
        assert!(
            names.contains(&"Page"),
            "missing function component, got {:?}",
            names
        );
    }
}
