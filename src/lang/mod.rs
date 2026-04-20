use tree_sitter::Language;

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
        let tree = parser
            .parse("pub fn main() !void {}", None)
            .unwrap();
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
        let tree = parser
            .parse("# Hello\n\nSome text.\n", None)
            .unwrap();
        assert_eq!(tree.root_node().kind(), "document");
        assert!(!tree.root_node().has_error());
    }
}
