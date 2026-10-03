//! This crate provides Jai language support for the [tree-sitter][] parsing library.
//!
//! It is tsift's packaging of
//! [constantitus/tree-sitter-jai](https://github.com/constantitus/tree-sitter-jai)
//! at commit `96440b0`, and exposes the same API as a standard tree-sitter
//! grammar crate so it can be swapped for an upstream `tree-sitter-jai` crate.
//!
//! Typically, you will use the [LANGUAGE][] constant to add this language to a
//! tree-sitter [Parser][], and then use the parser to parse some code:
//!
//! ```
//! let code = r#"
//! main :: () {
//!     print("Hello, Sailor!\n");
//! }
//! "#;
//! let mut parser = tree_sitter::Parser::new();
//! let language = tsift_tree_sitter_jai::LANGUAGE;
//! parser
//!     .set_language(&language.into())
//!     .expect("Error loading Jai parser");
//! let tree = parser.parse(code, None).unwrap();
//! assert!(!tree.root_node().has_error());
//! ```
//!
//! [Parser]: https://docs.rs/tree-sitter/*/tree_sitter/struct.Parser.html
//! [tree-sitter]: https://tree-sitter.github.io/

use tree_sitter_language::LanguageFn;

unsafe extern "C" {
    fn tree_sitter_jai() -> *const ();
}

/// The tree-sitter [`LanguageFn`][LanguageFn] for this grammar.
///
/// [LanguageFn]: https://docs.rs/tree-sitter-language/*/tree_sitter_language/struct.LanguageFn.html
pub const LANGUAGE: LanguageFn = unsafe { LanguageFn::from_raw(tree_sitter_jai) };

/// The content of the [`node-types.json`][] file for this grammar.
///
/// [`node-types.json`]: https://tree-sitter.github.io/tree-sitter/using-parsers#static-node-types
pub const NODE_TYPES: &str = include_str!("../../src/node-types.json");

#[cfg(test)]
mod tests {
    #[test]
    fn test_can_load_grammar() {
        let mut parser = tree_sitter::Parser::new();
        parser
            .set_language(&super::LANGUAGE.into())
            .expect("Error loading Jai parser");
    }

    #[test]
    fn test_parses_declarations_without_errors() {
        let mut parser = tree_sitter::Parser::new();
        parser.set_language(&super::LANGUAGE.into()).unwrap();
        let code = "#import \"Basic\";\n\
                    Vector2 :: struct { x: float; y: float; }\n\
                    length :: (v: Vector2) -> float { return sqrt(v.x * v.x + v.y * v.y); }\n";
        let tree = parser.parse(code, None).unwrap();
        let root = tree.root_node();
        assert!(!root.has_error(), "{}", root.to_sexp());
        assert_eq!(root.kind(), "source_file");
        assert!(root.named_child_count() >= 3, "{}", root.to_sexp());
    }

    #[test]
    fn test_node_types_is_json_array() {
        let trimmed = super::NODE_TYPES.trim_start();
        assert!(trimmed.starts_with('['));
        assert!(super::NODE_TYPES.contains("\"source_file\""));
    }
}
