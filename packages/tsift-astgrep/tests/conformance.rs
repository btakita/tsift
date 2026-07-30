//! Cross-language conformance suite for the structural tier.
//!
//! Every `AstGrepLang` variant gets one fixture row, and every row is put
//! through the same invariants. Per-language tests can only prove that the
//! language someone remembered to write a test for works; this suite makes the
//! *table* the unit of coverage, so adding a grammar without a fixture is a
//! failure rather than a silent gap.
//!
//! Grammar quirks (C and CSS needing a statement terminator, Dart and Solidity
//! matching only whole declarations) are recorded as row data rather than prose,
//! so a grammar upgrade that removes a limit is noticed instead of leaving a
//! stale note behind.

use tsift_astgrep::{AstGrepLang, rewrite_source, search_source};

/// How finely a grammar lets a standalone pattern select.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Granularity {
    /// Expression-shaped patterns match directly — the normal case.
    Expression,
    /// The grammar reads a bare call as a declaration, so the pattern needs the
    /// statement terminator to disambiguate.
    StatementTerminated,
    /// The grammar cannot parse an expression fragment as a standalone pattern
    /// at all; only whole declarations match.
    DeclarationOnly,
}

struct Case {
    lang: AstGrepLang,
    /// A filename whose extension must resolve back to `lang`.
    sample_path: &'static str,
    source: &'static str,
    /// Pattern that must match `expected_matches` times in `source`.
    pattern: &'static str,
    expected_matches: usize,
    /// Replacement for `pattern`; must change the buffer.
    rewrite: &'static str,
    /// Text that must appear in the rewritten buffer.
    rewrite_marker: &'static str,
    granularity: Granularity,
    /// Patterns that are known *not* to match this grammar. Empty for
    /// `Expression` languages; the pinned limit for the others.
    known_non_matching: &'static [&'static str],
}

const CASES: &[Case] = &[
    #[cfg(feature = "lang-rust")]
    Case {
        lang: AstGrepLang::Rust,
        sample_path: "src/lib.rs",
        source: "fn main() {\n    foo(1);\n    foo(2);\n}\n",
        pattern: "foo($A)",
        expected_matches: 2,
        rewrite: "bar($A)",
        rewrite_marker: "bar(1)",
        granularity: Granularity::Expression,
        known_non_matching: &[],
    },
    #[cfg(feature = "lang-python")]
    Case {
        lang: AstGrepLang::Python,
        sample_path: "app.py",
        source: "def main():\n    foo(1)\n    foo(2)\n",
        pattern: "foo($A)",
        expected_matches: 2,
        rewrite: "bar($A)",
        rewrite_marker: "bar(1)",
        granularity: Granularity::Expression,
        known_non_matching: &[],
    },
    #[cfg(feature = "lang-typescript")]
    Case {
        lang: AstGrepLang::TypeScript,
        sample_path: "app.ts",
        source: "function main(): void {\n  foo(1);\n  foo(2);\n}\n",
        pattern: "foo($A)",
        expected_matches: 2,
        rewrite: "bar($A)",
        rewrite_marker: "bar(1)",
        granularity: Granularity::Expression,
        known_non_matching: &[],
    },
    #[cfg(feature = "lang-typescript")]
    Case {
        lang: AstGrepLang::Tsx,
        sample_path: "App.tsx",
        source: "const App = () => {\n  foo(1);\n  foo(2);\n  return null;\n};\n",
        pattern: "foo($A)",
        expected_matches: 2,
        rewrite: "bar($A)",
        rewrite_marker: "bar(1)",
        granularity: Granularity::Expression,
        known_non_matching: &[],
    },
    #[cfg(feature = "lang-javascript")]
    Case {
        lang: AstGrepLang::JavaScript,
        sample_path: "app.js",
        source: "function main() {\n  foo(1);\n  foo(2);\n}\n",
        pattern: "foo($A)",
        expected_matches: 2,
        rewrite: "bar($A)",
        rewrite_marker: "bar(1)",
        granularity: Granularity::Expression,
        known_non_matching: &[],
    },
    #[cfg(feature = "lang-kotlin")]
    Case {
        lang: AstGrepLang::Kotlin,
        sample_path: "Main.kt",
        source: "fun main() {\n    foo(1)\n    foo(2)\n}\n",
        pattern: "foo($A)",
        expected_matches: 2,
        rewrite: "bar($A)",
        rewrite_marker: "bar(1)",
        granularity: Granularity::Expression,
        known_non_matching: &[],
    },
    #[cfg(feature = "lang-bash")]
    Case {
        lang: AstGrepLang::Bash,
        sample_path: "run.sh",
        source: "foo 1\nfoo 2\n",
        pattern: "foo $A",
        expected_matches: 2,
        rewrite: "bar $A",
        rewrite_marker: "bar 1",
        granularity: Granularity::Expression,
        known_non_matching: &[],
    },
    #[cfg(feature = "lang-markdown")]
    Case {
        lang: AstGrepLang::Markdown,
        sample_path: "README.md",
        source: "# a\n\n# b\n",
        pattern: "# $A",
        expected_matches: 2,
        rewrite: "## $A",
        rewrite_marker: "## a",
        granularity: Granularity::Expression,
        known_non_matching: &[],
    },
    #[cfg(feature = "lang-c")]
    Case {
        lang: AstGrepLang::C,
        sample_path: "main.c",
        // `foo($A)` reads as a declaration here — `foo` a type, `$A` a
        // declarator — so the terminator is what forces the call reading.
        source: "int main(void) {\n  foo(1);\n  foo(2);\n  return 0;\n}\n",
        pattern: "foo($A);",
        expected_matches: 2,
        rewrite: "bar($A);",
        rewrite_marker: "bar(1);",
        granularity: Granularity::StatementTerminated,
        known_non_matching: &["foo($A)"],
    },
    #[cfg(feature = "lang-cpp")]
    Case {
        lang: AstGrepLang::Cpp,
        sample_path: "main.cpp",
        source: "int main() {\n  foo(1);\n  foo(2);\n  return 0;\n}\n",
        pattern: "foo($A)",
        expected_matches: 2,
        rewrite: "bar($A)",
        rewrite_marker: "bar(1)",
        granularity: Granularity::Expression,
        known_non_matching: &[],
    },
    #[cfg(feature = "lang-csharp")]
    Case {
        lang: AstGrepLang::CSharp,
        sample_path: "Program.cs",
        source: "class C {\n  void M() {\n    Foo(1);\n    Foo(2);\n  }\n}\n",
        pattern: "Foo($A)",
        expected_matches: 2,
        rewrite: "Bar($A)",
        rewrite_marker: "Bar(1)",
        granularity: Granularity::Expression,
        known_non_matching: &[],
    },
    #[cfg(feature = "lang-css")]
    Case {
        lang: AstGrepLang::Css,
        sample_path: "site.css",
        source: "body { color: red; }\n.x { color: blue; }\n",
        pattern: "color: $V;",
        expected_matches: 2,
        rewrite: "background: $V;",
        rewrite_marker: "background: red;",
        granularity: Granularity::StatementTerminated,
        known_non_matching: &["color: $V"],
    },
    #[cfg(feature = "lang-dart")]
    Case {
        lang: AstGrepLang::Dart,
        sample_path: "main.dart",
        // tree-sitter-dart cannot parse a bare expression as a standalone
        // pattern, so there are no call-site codemods in Dart at all. This is
        // the sharpest limit in the structural tier.
        // Written on one line so the whole-declaration pattern is textually
        // identical to the source, keeping the identity-rewrite invariant
        // applicable here too.
        source: "void main() { print(\"a\"); }\n",
        pattern: "void main() { print($A); }",
        expected_matches: 1,
        rewrite: "void main() { log($A); }",
        rewrite_marker: "log(\"a\")",
        granularity: Granularity::DeclarationOnly,
        known_non_matching: &["print($A)", "print($A);", "print(\"a\")"],
    },
    #[cfg(feature = "lang-elixir")]
    Case {
        lang: AstGrepLang::Elixir,
        sample_path: "worker.ex",
        source: "defmodule M do\n  def run do\n    foo(1)\n    foo(2)\n  end\nend\n",
        pattern: "foo($A)",
        expected_matches: 2,
        rewrite: "bar($A)",
        rewrite_marker: "bar(1)",
        granularity: Granularity::Expression,
        known_non_matching: &[],
    },
    #[cfg(feature = "lang-go")]
    Case {
        lang: AstGrepLang::Go,
        sample_path: "main.go",
        source: "package main\n\nfunc main() {\n\tfoo(1)\n\tfoo(2)\n}\n",
        pattern: "foo($A)",
        expected_matches: 2,
        rewrite: "bar($A)",
        rewrite_marker: "bar(1)",
        granularity: Granularity::Expression,
        known_non_matching: &[],
    },
    #[cfg(feature = "lang-haskell")]
    Case {
        lang: AstGrepLang::Haskell,
        sample_path: "Main.hs",
        source: "main = do\n  foo 1\n  foo 2\n",
        pattern: "foo $A",
        expected_matches: 2,
        rewrite: "bar $A",
        rewrite_marker: "bar 1",
        granularity: Granularity::Expression,
        known_non_matching: &[],
    },
    #[cfg(feature = "lang-hcl")]
    Case {
        lang: AstGrepLang::Hcl,
        sample_path: "main.tf",
        // HCL has no expression statements: a call only exists as the right
        // hand side of an attribute, so the pattern has to carry the binding.
        source: "resource \"a\" \"b\" {\n  x = foo(1)\n  y = foo(2)\n}\n",
        pattern: "$K = foo($A)",
        expected_matches: 2,
        rewrite: "$K = bar($A)",
        rewrite_marker: "x = bar(1)",
        granularity: Granularity::StatementTerminated,
        known_non_matching: &["foo($A)"],
    },
    #[cfg(feature = "lang-html")]
    Case {
        lang: AstGrepLang::Html,
        sample_path: "index.html",
        source: "<div><span>a</span><span>b</span></div>\n",
        pattern: "<span>$A</span>",
        expected_matches: 2,
        rewrite: "<em>$A</em>",
        rewrite_marker: "<em>a</em>",
        granularity: Granularity::Expression,
        known_non_matching: &[],
    },
    #[cfg(feature = "lang-java")]
    Case {
        lang: AstGrepLang::Java,
        sample_path: "C.java",
        source: "class C {\n  void m() {\n    foo(1);\n    foo(2);\n  }\n}\n",
        pattern: "foo($A)",
        expected_matches: 2,
        rewrite: "bar($A)",
        rewrite_marker: "bar(1)",
        granularity: Granularity::Expression,
        known_non_matching: &[],
    },
    #[cfg(feature = "lang-json")]
    Case {
        lang: AstGrepLang::Json,
        sample_path: "data.json",
        // A literal key with a metavariable value (`"a": $V`) parses to two
        // nodes in tree-sitter-json and is refused; the pair has to be
        // metavariable-shaped on both sides.
        source: "{\"a\": 1, \"b\": 1}\n",
        pattern: "$K: $V",
        expected_matches: 2,
        rewrite: "$K: 2",
        rewrite_marker: "\"a\": 2",
        granularity: Granularity::Expression,
        known_non_matching: &[],
    },
    #[cfg(feature = "lang-lua")]
    Case {
        lang: AstGrepLang::Lua,
        sample_path: "init.lua",
        source: "foo(1)\nfoo(2)\n",
        pattern: "foo($A)",
        expected_matches: 2,
        rewrite: "bar($A)",
        rewrite_marker: "bar(1)",
        granularity: Granularity::Expression,
        known_non_matching: &[],
    },
    #[cfg(feature = "lang-nix")]
    Case {
        lang: AstGrepLang::Nix,
        sample_path: "default.nix",
        source: "{ a = foo 1; b = foo 2; }\n",
        pattern: "foo $A",
        expected_matches: 2,
        rewrite: "bar $A",
        rewrite_marker: "bar 1",
        granularity: Granularity::Expression,
        known_non_matching: &[],
    },
    #[cfg(feature = "lang-php")]
    Case {
        lang: AstGrepLang::Php,
        sample_path: "index.php",
        source: "<?php\nfoo(1);\nfoo(2);\n",
        pattern: "foo($A)",
        expected_matches: 2,
        rewrite: "bar($A)",
        rewrite_marker: "bar(1)",
        granularity: Granularity::Expression,
        known_non_matching: &[],
    },
    #[cfg(feature = "lang-ruby")]
    Case {
        lang: AstGrepLang::Ruby,
        sample_path: "app.rb",
        source: "foo(1)\nfoo(2)\n",
        pattern: "foo($A)",
        expected_matches: 2,
        rewrite: "bar($A)",
        rewrite_marker: "bar(1)",
        granularity: Granularity::Expression,
        known_non_matching: &[],
    },
    #[cfg(feature = "lang-scala")]
    Case {
        lang: AstGrepLang::Scala,
        sample_path: "M.scala",
        source: "object M {\n  def run(): Unit = {\n    foo(1)\n    foo(2)\n  }\n}\n",
        pattern: "foo($A)",
        expected_matches: 2,
        rewrite: "bar($A)",
        rewrite_marker: "bar(1)",
        granularity: Granularity::Expression,
        known_non_matching: &[],
    },
    #[cfg(feature = "lang-solidity")]
    Case {
        lang: AstGrepLang::Solidity,
        sample_path: "C.sol",
        source: "contract C {\n  function f() public { emit E(1); }\n}\n",
        pattern: "function f() public { $$$B }",
        expected_matches: 1,
        rewrite: "function g() public { $$$B }",
        rewrite_marker: "function g() public",
        granularity: Granularity::DeclarationOnly,
        known_non_matching: &["emit E($A)", "emit E($A);", "E($A)"],
    },
    #[cfg(feature = "lang-swift")]
    Case {
        lang: AstGrepLang::Swift,
        sample_path: "main.swift",
        source: "func main() {\n    foo(1)\n    foo(2)\n}\n",
        pattern: "foo($A)",
        expected_matches: 2,
        rewrite: "bar($A)",
        rewrite_marker: "bar(1)",
        granularity: Granularity::Expression,
        known_non_matching: &[],
    },
    #[cfg(feature = "lang-yaml")]
    Case {
        lang: AstGrepLang::Yaml,
        sample_path: "config.yaml",
        source: "a: 1\nb: 1\n",
        pattern: "$K: 1",
        expected_matches: 2,
        rewrite: "$K: 2",
        rewrite_marker: "a: 2",
        granularity: Granularity::Expression,
        known_non_matching: &[],
    },
];

/// A pattern no fixture source contains, used to prove the no-match path.
const NEVER_MATCHES: &str = "tsift_conformance_absent_symbol($A)";

/// Invariants every supported language must satisfy identically.
///
/// Returns the number of matches the library actually produced, so the caller
/// can prove the rows ran against real output rather than counting loop turns.
fn assert_conformance(case: &Case) -> usize {
    let lang = case.lang;
    let name = lang.name();

    // A row that asserts zero matches would pass while proving nothing.
    assert!(
        case.expected_matches > 0,
        "{name}: fixture must assert at least one match"
    );

    // Resolution: the row's own filename and canonical name must round-trip.
    assert_eq!(
        AstGrepLang::from_path(std::path::Path::new(case.sample_path)),
        Some(lang),
        "{name}: sample path {} does not resolve to this language",
        case.sample_path
    );
    assert_eq!(
        AstGrepLang::from_name(name),
        Some(lang),
        "{name}: canonical name does not round-trip"
    );

    // Search: exact count, and every span must be a real slice of the source.
    let hits = search_source(case.source, lang, case.pattern)
        .unwrap_or_else(|err| panic!("{name}: search failed: {err}"));
    assert_eq!(
        hits.len(),
        case.expected_matches,
        "{name}: pattern `{}` matched {} time(s), expected {}",
        case.pattern,
        hits.len(),
        case.expected_matches
    );
    for hit in &hits {
        assert!(
            hit.start_byte < hit.end_byte && hit.end_byte <= case.source.len(),
            "{name}: match span {}..{} is outside the source",
            hit.start_byte,
            hit.end_byte
        );
        assert_eq!(
            &case.source[hit.start_byte..hit.end_byte],
            hit.text,
            "{name}: reported text does not match the reported byte span"
        );
        assert!(hit.start_line >= 1, "{name}: lines are 1-based");
        assert!(
            hit.start_line <= hit.end_line,
            "{name}: match ends before it starts"
        );
    }

    // Rewrite: same count, buffer actually changes, replacement lands.
    let out = rewrite_source(case.source, lang, case.pattern, case.rewrite)
        .unwrap_or_else(|err| panic!("{name}: rewrite failed: {err}"));
    assert_eq!(
        out.replacements, case.expected_matches,
        "{name}: rewrote {} match(es), expected {}",
        out.replacements, case.expected_matches
    );
    assert!(!out.unchanged, "{name}: rewrite reported no textual change");
    assert_ne!(out.source, case.source, "{name}: rewrite left the buffer as-is");
    assert!(
        out.source.contains(case.rewrite_marker),
        "{name}: rewritten buffer is missing `{}`:\n{}",
        case.rewrite_marker,
        out.source
    );

    // A pattern that matches nothing must leave the buffer byte-identical.
    let miss = rewrite_source(case.source, lang, NEVER_MATCHES, "x")
        .unwrap_or_else(|err| panic!("{name}: no-match rewrite failed: {err}"));
    assert_eq!(miss.replacements, 0, "{name}: absent pattern matched");
    assert_eq!(miss.source, case.source, "{name}: absent pattern edited the buffer");
    assert!(!miss.unchanged, "{name}: `unchanged` must mean matched-but-no-op");

    // Rewriting a pattern with itself matched but changed nothing — the signal
    // that separates a no-op codemod from a miss.
    let identity = rewrite_source(case.source, lang, case.pattern, case.pattern)
        .unwrap_or_else(|err| panic!("{name}: identity rewrite failed: {err}"));
    assert_eq!(identity.replacements, case.expected_matches);
    assert!(
        identity.unchanged,
        "{name}: identity rewrite should report unchanged"
    );

    // An empty pattern is a refusal on every language.
    assert!(
        search_source(case.source, lang, "   ").is_err(),
        "{name}: empty pattern was accepted"
    );

    // Known limits stay pinned: if one of these starts matching, the grammar
    // improved and the recorded granularity is stale.
    match case.granularity {
        Granularity::Expression => assert!(
            case.known_non_matching.is_empty(),
            "{name}: an expression-granularity language should have no pinned limits"
        ),
        Granularity::StatementTerminated | Granularity::DeclarationOnly => assert!(
            !case.known_non_matching.is_empty(),
            "{name}: a restricted-granularity language must pin what does not match"
        ),
    }
    for pattern in case.known_non_matching {
        let hits = search_source(case.source, lang, pattern)
            .unwrap_or_else(|err| panic!("{name}: pinned-limit search failed: {err}"));
        assert!(
            hits.is_empty(),
            "{name}: `{pattern}` now matches ({} hit(s)) — the recorded {:?} limit is stale",
            hits.len(),
            case.granularity
        );
    }

    hits.len()
}

#[test]
fn every_supported_language_has_exactly_one_conformance_fixture() {
    let mut fixtures: Vec<&str> = CASES.iter().map(|case| case.lang.name()).collect();
    fixtures.sort_unstable();
    let mut before_dedup = fixtures.len();
    fixtures.dedup();
    assert_eq!(
        fixtures.len(),
        before_dedup,
        "duplicate conformance fixture rows: {fixtures:?}"
    );

    let mut supported: Vec<&str> = AstGrepLang::all().iter().map(|lang| lang.name()).collect();
    supported.sort_unstable();
    before_dedup = supported.len();
    supported.dedup();
    assert_eq!(supported.len(), before_dedup, "AstGrepLang::all() has duplicates");

    assert_eq!(
        fixtures, supported,
        "conformance fixtures and supported languages disagree — a language was \
         added or removed without its fixture row"
    );
}

/// The workspace build enables `all-languages` through `tsift-cli`, so this is
/// the anchor that keeps the suite from passing over an empty table.
#[cfg(feature = "all-languages")]
#[test]
fn the_full_language_set_is_covered_and_not_a_handful() {
    assert_eq!(
        CASES.len(),
        AstGrepLang::all().len(),
        "fixture count and language count disagree"
    );
    assert!(
        CASES.len() >= 28,
        "expected the full structural tier, got {} language(s)",
        CASES.len()
    );
}

#[test]
fn every_language_satisfies_the_shared_structural_contract() {
    let mut rows_run = 0usize;
    let mut matches_seen = 0usize;
    for case in CASES {
        matches_seen += assert_conformance(case);
        rows_run += 1;
    }

    assert_eq!(rows_run, CASES.len(), "not every fixture row ran");
    let expected: usize = CASES.iter().map(|case| case.expected_matches).sum();
    assert_eq!(
        matches_seen, expected,
        "the matches the library produced do not add up to what the table declares"
    );
    assert!(
        matches_seen >= CASES.len(),
        "every row must contribute at least one real match"
    );
}

/// A pattern that is merely invalid for a grammar used to abort the process:
/// `ast-grep-core`'s `&str` matcher path `unwrap()`s the parse. It must be a
/// refusal on every language instead.
#[test]
fn an_unparseable_pattern_is_refused_on_every_language() {
    let mut refused = 0usize;
    for case in CASES {
        let name = case.lang.name();
        // Two statements — `MultipleNode` in every grammar that has statements.
        let err = search_source(case.source, case.lang, "foo(1); foo(2);")
            .err()
            .map(|err| err.to_string());
        // Grammars where that text parses to a single node (or to nothing at
        // all) legitimately have no error; what must never happen is a panic.
        if let Some(err) = err {
            assert!(
                err.contains("invalid structural pattern"),
                "{name}: unexpected error shape: {err}"
            );
            assert!(err.contains(name), "{name}: error does not name the language");
            refused += 1;
        }
    }
    assert!(
        refused > 0,
        "no language refused a multi-node pattern — the guard is not reachable"
    );
}
