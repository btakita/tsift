//! Cross-language conformance suite for the indexed tier.
//!
//! The per-language `test_extract_*` unit tests each prove one language works.
//! None of them notices a *new* language arriving with no test at all, and none
//! of them checks the invariants that must hold identically everywhere: spans
//! inside the source, sorted output, an empty file producing an empty list, a
//! truncated file not panicking. This suite makes the table the unit of
//! coverage, so a `Lang` variant without a fixture row is a failure.

use tsift_graph::{Lang, Symbol};

struct Case {
    lang: Lang,
    /// Every extension that must resolve to this language.
    extensions: &'static [&'static str],
    source: &'static str,
    /// A symbol the extractor must find, and the kind it must report.
    expect_name: &'static str,
    expect_kind: &'static str,
    /// A source that is syntactically broken for this language. Extraction must
    /// return rather than panic; tree-sitter recovers, so results are ignored.
    truncated: &'static str,
}

const CASES: &[Case] = &[
    #[cfg(feature = "lang-rust")]
    Case {
        lang: Lang::Rust,
        extensions: &["rs"],
        source: "fn main() {}\nstruct Foo;\n",
        expect_name: "main",
        expect_kind: "function",
        truncated: "fn main( { struct",
    },
    #[cfg(feature = "lang-python")]
    Case {
        lang: Lang::Python,
        extensions: &["py", "pyi"],
        source: "def hello():\n    pass\n\nclass Widget:\n    pass\n",
        expect_name: "hello",
        expect_kind: "function",
        truncated: "def hello(:\n  class",
    },
    #[cfg(feature = "lang-typescript")]
    Case {
        lang: Lang::TypeScript,
        extensions: &["ts"],
        source: "function greet(): void {}\nclass Foo {}\n",
        expect_name: "greet",
        expect_kind: "function",
        truncated: "function greet(: {",
    },
    #[cfg(feature = "lang-typescript")]
    Case {
        lang: Lang::Tsx,
        extensions: &["tsx"],
        source: "export const App = () => null;\nclass Foo {}\n",
        expect_name: "App",
        expect_kind: "function",
        truncated: "export const App = ( =>",
    },
    #[cfg(feature = "lang-javascript")]
    Case {
        lang: Lang::JavaScript,
        extensions: &["js", "mjs", "cjs"],
        source: "function hello() {}\nclass Widget {}\n",
        expect_name: "hello",
        expect_kind: "function",
        truncated: "function hello( {",
    },
    #[cfg(feature = "lang-javascript")]
    Case {
        lang: Lang::Jsx,
        extensions: &["jsx"],
        source: "export const App = () => null;\nclass Widget {}\n",
        expect_name: "App",
        expect_kind: "function",
        truncated: "export const App = ( =>",
    },
    #[cfg(feature = "lang-kotlin")]
    Case {
        lang: Lang::Kotlin,
        extensions: &["kt", "kts"],
        source: "fun main() {}\nclass Foo\n",
        expect_name: "main",
        expect_kind: "function",
        truncated: "fun main( { class",
    },
    #[cfg(feature = "lang-zig")]
    Case {
        lang: Lang::Zig,
        extensions: &["zig"],
        source: "pub fn main() void {}\n",
        expect_name: "main",
        expect_kind: "function",
        truncated: "pub fn main( void {",
    },
    #[cfg(feature = "lang-bash")]
    Case {
        lang: Lang::Bash,
        extensions: &["sh", "bash", "zsh"],
        source: "run_it() {\n  echo hi\n}\n",
        expect_name: "run_it",
        expect_kind: "function",
        truncated: "run_it( {\n echo",
    },
    #[cfg(feature = "lang-markdown")]
    Case {
        lang: Lang::Markdown,
        extensions: &["md", "mdx"],
        source: "# Title\n\nbody text\n",
        expect_name: "Title",
        expect_kind: "heading",
        truncated: "```\nunclosed fence\n",
    },
];

/// Invariants that must hold for every extracted symbol, in every language.
fn assert_symbol_shape(lang: Lang, source: &str, symbols: &[Symbol]) {
    let name = lang.name();
    for symbol in symbols {
        assert!(!symbol.name.is_empty(), "{name}: symbol with an empty name");
        assert!(!symbol.kind.is_empty(), "{name}: symbol with an empty kind");
        assert!(
            symbol.line <= symbol.end_line,
            "{name}: symbol {} ends before it starts",
            symbol.name
        );
        assert!(
            symbol.start_byte < symbol.end_byte,
            "{name}: symbol {} has an empty byte span",
            symbol.name
        );
        assert!(
            symbol.end_byte <= source.len(),
            "{name}: symbol {} spans past the end of the source",
            symbol.name
        );
        assert!(
            source.is_char_boundary(symbol.start_byte) && source.is_char_boundary(symbol.end_byte),
            "{name}: symbol {} span is not on UTF-8 boundaries",
            symbol.name
        );
        if let (Some(body_start), Some(body_end)) = (symbol.body_start_byte, symbol.body_end_byte) {
            assert!(
                symbol.start_byte <= body_start && body_start <= body_end,
                "{name}: symbol {} has a body span outside its own span",
                symbol.name
            );
            assert!(
                body_end <= symbol.end_byte,
                "{name}: symbol {} body ends past the symbol",
                symbol.name
            );
        }
    }

    // `extract_symbols` promises line-ordered output; downstream ranking and
    // span slicing rely on it.
    let mut previous = 0usize;
    for symbol in symbols {
        assert!(
            symbol.line >= previous,
            "{name}: symbols are not line-ordered ({} at {} follows {})",
            symbol.name,
            symbol.line,
            previous
        );
        previous = symbol.line;
    }
}

/// Returns the number of symbols the extractor produced, so the caller can
/// prove rows ran against real output rather than counting loop turns.
fn assert_conformance(case: &Case) -> usize {
    let lang = case.lang;
    let name = lang.name();

    assert!(
        !case.extensions.is_empty(),
        "{name}: a language reachable from no extension is never indexed"
    );
    for ext in case.extensions {
        assert_eq!(
            Lang::from_extension(ext),
            Some(lang),
            "{name}: .{ext} does not resolve to this language"
        );
    }

    // The grammar must load and both queries must compile against it.
    let ts_lang = lang.tree_sitter_language();
    let mut parser = tree_sitter::Parser::new();
    parser
        .set_language(&ts_lang)
        .unwrap_or_else(|err| panic!("{name}: grammar rejected by the parser: {err}"));
    tree_sitter::Query::new(&ts_lang, lang.symbol_query())
        .unwrap_or_else(|err| panic!("{name}: symbol query does not compile: {err}"));
    if let Some(query) = lang.call_query() {
        tree_sitter::Query::new(&ts_lang, query)
            .unwrap_or_else(|err| panic!("{name}: call query does not compile: {err}"));
    }

    let symbols = lang
        .extract_symbols(case.source.as_bytes())
        .unwrap_or_else(|err| panic!("{name}: extraction failed: {err}"));
    assert!(
        !symbols.is_empty(),
        "{name}: fixture produced no symbols at all"
    );
    assert_symbol_shape(lang, case.source, &symbols);

    let found = symbols
        .iter()
        .find(|symbol| symbol.name == case.expect_name)
        .unwrap_or_else(|| {
            panic!(
                "{name}: expected symbol `{}`, got {:?}",
                case.expect_name,
                symbols.iter().map(|s| &s.name).collect::<Vec<_>>()
            )
        });
    assert_eq!(
        found.kind, case.expect_kind,
        "{name}: `{}` reported as `{}`, expected `{}`",
        case.expect_name, found.kind, case.expect_kind
    );
    // The reported span has to actually contain the symbol it names, otherwise
    // every `source-read`/`symbol-read` window built from it is wrong.
    assert!(
        case.source[found.start_byte..found.end_byte].contains(case.expect_name),
        "{name}: span for `{}` does not contain its own name",
        case.expect_name
    );

    // An empty file is not an error, and has nothing in it.
    let empty = lang
        .extract_symbols(b"")
        .unwrap_or_else(|err| panic!("{name}: empty source errored: {err}"));
    assert!(empty.is_empty(), "{name}: empty source produced symbols");

    // Broken syntax must not panic. tree-sitter error-recovers, so whatever
    // comes back still has to satisfy the shared span invariants.
    let recovered = lang
        .extract_symbols(case.truncated.as_bytes())
        .unwrap_or_else(|err| panic!("{name}: truncated source errored: {err}"));
    assert_symbol_shape(lang, case.truncated, &recovered);

    symbols.len()
}

#[test]
fn every_indexed_language_has_exactly_one_conformance_fixture() {
    let mut fixtures: Vec<&str> = CASES.iter().map(|case| case.lang.name()).collect();
    fixtures.sort_unstable();
    let before = fixtures.len();
    fixtures.dedup();
    assert_eq!(
        fixtures.len(),
        before,
        "duplicate conformance fixture rows: {fixtures:?}"
    );

    let mut supported: Vec<&str> = Lang::all().iter().map(|lang| lang.name()).collect();
    supported.sort_unstable();
    let before = supported.len();
    supported.dedup();
    assert_eq!(
        supported.len(),
        before,
        "two Lang variants share a name — every `name()` must be distinct"
    );

    assert_eq!(
        fixtures, supported,
        "conformance fixtures and indexed languages disagree — a language was \
         added or removed without its fixture row"
    );
    assert!(
        !fixtures.is_empty(),
        "no indexed languages compiled in; this suite would prove nothing"
    );
}

#[test]
fn every_indexed_language_satisfies_the_shared_extraction_contract() {
    let mut rows_run = 0usize;
    let mut symbols_seen = 0usize;
    for case in CASES {
        symbols_seen += assert_conformance(case);
        rows_run += 1;
    }

    assert_eq!(rows_run, CASES.len(), "not every fixture row ran");
    assert!(
        symbols_seen >= CASES.len(),
        "every row must contribute at least one real symbol ({symbols_seen} from {} rows)",
        CASES.len()
    );
}

/// An unknown extension resolves to nothing rather than to a default language —
/// silently parsing an unrelated file with the wrong grammar is worse than a
/// miss, because it produces plausible symbols nobody asked for.
#[test]
fn unknown_extensions_resolve_to_no_language() {
    for ext in ["", "txt", "bin", "xyz", "RS"] {
        assert_eq!(
            Lang::from_extension(ext),
            None,
            "extension {ext:?} unexpectedly resolved to a language"
        );
    }
}
