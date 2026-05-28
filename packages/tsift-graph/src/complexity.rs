use anyhow::Result;
use std::collections::HashMap;
use tree_sitter::{Parser, Query, QueryCursor, StreamingIterator};

use crate::lang::Lang;

#[derive(Debug, Clone, Default)]
pub struct ComplexityMetrics {
    pub branches: i64,
    pub loops: i64,
    pub returns: i64,
    pub max_nesting: i64,
    pub unsafe_blocks: i64,
}

impl ComplexityMetrics {
    pub fn total_complexity(&self) -> i64 {
        self.branches + self.loops + self.returns
    }

    pub fn from_raw_fields(
        branches: i64,
        loops: i64,
        returns: i64,
        max_nesting: i64,
        unsafe_blocks: i64,
    ) -> Self {
        Self {
            branches,
            loops,
            returns,
            max_nesting,
            unsafe_blocks,
        }
    }
}

pub trait LanguageExtractor: Send + Sync {
    fn lang(&self) -> Lang;
    fn extract_complexity(&self, source: &[u8]) -> Result<ComplexityMetrics>;
}

struct BuiltinExtractor {
    lang: Lang,
}

impl BuiltinExtractor {
    fn complexity_query(&self) -> Option<&'static str> {
        match self.lang {
            #[cfg(feature = "lang-rust")]
            Lang::Rust => Some(
                r#"
                (if_expression) @branch
                (match_expression) @branch
                (for_expression) @loop
                (while_expression) @loop
                (loop_expression) @loop
                (return_expression) @return
                (unsafe_block) @unsafe
            "#,
            ),
            #[cfg(feature = "lang-python")]
            Lang::Python => Some(
                r#"
                (if_statement) @branch
                (elif_clause) @branch
                (for_statement) @loop
                (while_statement) @loop
                (return_statement) @return
            "#,
            ),
            #[cfg(feature = "lang-typescript")]
            Lang::TypeScript | Lang::Tsx => Some(
                r#"
                (if_statement) @branch
                (switch_statement) @branch
                (ternary_expression) @branch
                (for_statement) @loop
                (for_in_statement) @loop
                (while_statement) @loop
                (do_statement) @loop
                (return_statement) @return
            "#,
            ),
            #[cfg(feature = "lang-javascript")]
            Lang::JavaScript | Lang::Jsx => Some(
                r#"
                (if_statement) @branch
                (switch_statement) @branch
                (ternary_expression) @branch
                (for_statement) @loop
                (for_in_statement) @loop
                (while_statement) @loop
                (do_statement) @loop
                (return_statement) @return
            "#,
            ),
            #[cfg(feature = "lang-kotlin")]
            Lang::Kotlin => Some(
                r#"
                (if_expression) @branch
                (when_expression) @branch
                (for_statement) @loop
                (while_statement) @loop
                (do_while_statement) @loop
                (return_expression) @return
            "#,
            ),
            _ => None,
        }
    }

    fn compute_max_nesting(&self, source: &[u8]) -> i64 {
        let ts_lang = self.lang.tree_sitter_language();
        let mut parser = Parser::new();
        if parser.set_language(&ts_lang).is_err() {
            return 0;
        }
        let tree = match parser.parse(source, None) {
            Some(t) => t,
            None => return 0,
        };
        let mut max_depth: i64 = 0;
        fn walk(node: tree_sitter::Node, depth: i64, max_depth: &mut i64) {
            let kind = node.kind();
            let is_scope = matches!(
                kind,
                "function_item"
                    | "function_definition"
                    | "function_declaration"
                    | "class_definition"
                    | "class_declaration"
                    | "impl_item"
                    | "if_expression"
                    | "if_statement"
                    | "for_expression"
                    | "for_statement"
                    | "while_expression"
                    | "while_statement"
                    | "loop_expression"
                    | "match_expression"
                    | "switch_statement"
                    | "when_expression"
                    | "block"
                    | "expression_list"
            );
            let child_depth = if is_scope { depth + 1 } else { depth };
            if child_depth > *max_depth {
                *max_depth = child_depth;
            }
            let mut cursor = node.walk();
            for child in node.children(&mut cursor) {
                walk(child, child_depth, max_depth);
            }
        }
        walk(tree.root_node(), 0, &mut max_depth);
        max_depth.max(0)
    }
}

impl LanguageExtractor for BuiltinExtractor {
    fn lang(&self) -> Lang {
        self.lang
    }

    fn extract_complexity(&self, source: &[u8]) -> Result<ComplexityMetrics> {
        let query_str = match self.complexity_query() {
            Some(q) => q,
            None => return Ok(ComplexityMetrics::default()),
        };
        let ts_lang = self.lang.tree_sitter_language();
        let mut parser = Parser::new();
        parser.set_language(&ts_lang)?;
        let tree = parser
            .parse(source, None)
            .ok_or_else(|| anyhow::anyhow!("parse failed"))?;
        let query = Query::new(&ts_lang, query_str)?;
        let mut cursor = QueryCursor::new();
        let mut metrics = ComplexityMetrics::default();

        let capture_names: Vec<String> = query
            .capture_names()
            .iter()
            .map(|s| s.to_string())
            .collect();

        let mut matches = cursor.matches(&query, tree.root_node(), source);
        while let Some(m) = matches.next() {
            for capture in m.captures {
                let name = &capture_names[capture.index as usize];
                match name.as_str() {
                    "branch" => metrics.branches += 1,
                    "loop" => metrics.loops += 1,
                    "return" => metrics.returns += 1,
                    "unsafe" => metrics.unsafe_blocks += 1,
                    _ => {}
                }
            }
        }

        metrics.max_nesting = self.compute_max_nesting(source);
        Ok(metrics)
    }
}

pub struct LanguageRegistry {
    extractors: HashMap<String, Box<dyn LanguageExtractor>>,
}

impl LanguageRegistry {
    pub fn new() -> Self {
        let mut registry = Self {
            extractors: HashMap::new(),
        };
        registry.register_builtins();
        registry
    }

    fn register_builtins(&mut self) {
        for lang in Lang::all() {
            let ext = lang.name().to_string();
            let extractor = BuiltinExtractor { lang };
            self.extractors.insert(ext, Box::new(extractor));
        }
    }

    pub fn register(&mut self, name: String, extractor: Box<dyn LanguageExtractor>) {
        self.extractors.insert(name, extractor);
    }

    pub fn get(&self, lang_name: &str) -> Option<&dyn LanguageExtractor> {
        self.extractors.get(lang_name).map(|e| e.as_ref())
    }

    pub fn extractor_for_extension(&self, ext: &str) -> Option<&dyn LanguageExtractor> {
        let lang = Lang::from_extension(ext)?;
        self.get(lang.name())
    }

    pub fn complexity_for_source(&self, lang: Lang, source: &[u8]) -> Result<ComplexityMetrics> {
        let extractor = self
            .get(lang.name())
            .ok_or_else(|| anyhow::anyhow!("no extractor registered for language: {}", lang.name()))?;
        extractor.extract_complexity(source)
    }

    pub fn registered_languages(&self) -> Vec<&str> {
        let mut names: Vec<&str> = self.extractors.keys().map(|s| s.as_str()).collect();
        names.sort();
        names
    }
}

impl Default for LanguageRegistry {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn registry_has_all_builtin_languages() {
        let registry = LanguageRegistry::new();
        let languages = registry.registered_languages();
        for lang in Lang::all() {
            assert!(
                languages.contains(&lang.name()),
                "missing builtin language: {}",
                lang.name()
            );
        }
    }

    #[cfg(feature = "lang-rust")]
    #[test]
    fn rust_complexity_counting() {
        let registry = LanguageRegistry::new();
        let source = br#"fn example(x: i32) -> i32 {
    if x > 0 {
        return x;
    }
    for i in 0..x {
        if i % 2 == 0 {
            continue;
        }
    }
    0
}
"#;
        let metrics = registry.complexity_for_source(Lang::Rust, source).unwrap();
        assert!(metrics.branches >= 2, "expected >=2 branches, got {}", metrics.branches);
        assert!(metrics.loops >= 1, "expected >=1 loop, got {}", metrics.loops);
        assert!(metrics.returns >= 1, "expected >=1 return, got {}", metrics.returns);
    }

    #[cfg(feature = "lang-python")]
    #[test]
    fn python_complexity_counting() {
        let registry = LanguageRegistry::new();
        let source = br#"def example(x):
    if x > 0:
        return x
    for i in range(x):
        if i % 2 == 0:
            continue
    return 0
"#;
        let metrics = registry.complexity_for_source(Lang::Python, source).unwrap();
        assert!(metrics.branches >= 2, "expected >=2 branches, got {}", metrics.branches);
        assert!(metrics.loops >= 1, "expected >=1 loop, got {}", metrics.loops);
        assert!(metrics.returns >= 2, "expected >=2 returns, got {}", metrics.returns);
    }

    #[cfg(feature = "lang-typescript")]
    #[test]
    fn typescript_complexity_counting() {
        let registry = LanguageRegistry::new();
        let source = br#"function example(x: number): number {
    if (x > 0) {
        return x;
    }
    for (let i = 0; i < x; i++) {
        if (i % 2 === 0) continue;
    }
    return 0;
}
"#;
        let metrics = registry
            .complexity_for_source(Lang::TypeScript, source)
            .unwrap();
        assert!(metrics.branches >= 2, "expected >=2 branches, got {}", metrics.branches);
        assert!(metrics.loops >= 1, "expected >=1 loop, got {}", metrics.loops);
        assert!(metrics.returns >= 2, "expected >=2 returns, got {}", metrics.returns);
    }

    #[test]
    fn total_complexity_sums_metrics() {
        let metrics = ComplexityMetrics::from_raw_fields(3, 2, 1, 4, 0);
        assert_eq!(metrics.total_complexity(), 6);
    }

    #[test]
    fn extractor_for_extension_works() {
        let registry = LanguageRegistry::new();
        assert!(registry.extractor_for_extension("rs").is_some());
        assert!(registry.extractor_for_extension("py").is_some());
        assert!(registry.extractor_for_extension("xyz").is_none());
    }
}
