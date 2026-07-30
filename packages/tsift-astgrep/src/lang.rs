//! Language resolution for structural patterns.
//!
//! `tsift-astgrep` deliberately exposes its own language enum instead of
//! re-exporting [`ast_grep_language::SupportLang`]. `SupportLang` declares every
//! variant unconditionally, but a variant whose grammar feature is disabled has
//! no usable parser, so handing one to the engine is a latent panic. Gating our
//! own enum at the type level means a disabled language fails resolution up
//! front with a listable set of supported names.

use serde::{Deserialize, Serialize};
use std::path::Path;

/// A language whose grammar is compiled into this build.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum AstGrepLang {
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
    #[cfg(feature = "lang-kotlin")]
    Kotlin,
    #[cfg(feature = "lang-bash")]
    Bash,
    #[cfg(feature = "lang-markdown")]
    Markdown,
    // Structural-only languages: matchable and rewritable, but not indexable by
    // `tsift-graph`/`tsift-search` and therefore not semantic-edit executors.
    #[cfg(feature = "lang-c")]
    C,
    #[cfg(feature = "lang-cpp")]
    Cpp,
    #[cfg(feature = "lang-csharp")]
    CSharp,
    #[cfg(feature = "lang-css")]
    Css,
    #[cfg(feature = "lang-dart")]
    Dart,
    #[cfg(feature = "lang-elixir")]
    Elixir,
    #[cfg(feature = "lang-go")]
    Go,
    #[cfg(feature = "lang-haskell")]
    Haskell,
    #[cfg(feature = "lang-hcl")]
    Hcl,
    #[cfg(feature = "lang-html")]
    Html,
    #[cfg(feature = "lang-java")]
    Java,
    #[cfg(feature = "lang-json")]
    Json,
    #[cfg(feature = "lang-lua")]
    Lua,
    #[cfg(feature = "lang-nix")]
    Nix,
    #[cfg(feature = "lang-php")]
    Php,
    #[cfg(feature = "lang-ruby")]
    Ruby,
    #[cfg(feature = "lang-scala")]
    Scala,
    #[cfg(feature = "lang-solidity")]
    Solidity,
    #[cfg(feature = "lang-swift")]
    Swift,
    #[cfg(feature = "lang-yaml")]
    Yaml,
}

impl AstGrepLang {
    /// Every language compiled into this build, in stable display order.
    pub fn all() -> &'static [AstGrepLang] {
        &[
            #[cfg(feature = "lang-rust")]
            AstGrepLang::Rust,
            #[cfg(feature = "lang-python")]
            AstGrepLang::Python,
            #[cfg(feature = "lang-typescript")]
            AstGrepLang::TypeScript,
            #[cfg(feature = "lang-typescript")]
            AstGrepLang::Tsx,
            #[cfg(feature = "lang-javascript")]
            AstGrepLang::JavaScript,
            #[cfg(feature = "lang-kotlin")]
            AstGrepLang::Kotlin,
            #[cfg(feature = "lang-bash")]
            AstGrepLang::Bash,
            #[cfg(feature = "lang-markdown")]
            AstGrepLang::Markdown,
            #[cfg(feature = "lang-c")]
            AstGrepLang::C,
            #[cfg(feature = "lang-cpp")]
            AstGrepLang::Cpp,
            #[cfg(feature = "lang-csharp")]
            AstGrepLang::CSharp,
            #[cfg(feature = "lang-css")]
            AstGrepLang::Css,
            #[cfg(feature = "lang-dart")]
            AstGrepLang::Dart,
            #[cfg(feature = "lang-elixir")]
            AstGrepLang::Elixir,
            #[cfg(feature = "lang-go")]
            AstGrepLang::Go,
            #[cfg(feature = "lang-haskell")]
            AstGrepLang::Haskell,
            #[cfg(feature = "lang-hcl")]
            AstGrepLang::Hcl,
            #[cfg(feature = "lang-html")]
            AstGrepLang::Html,
            #[cfg(feature = "lang-java")]
            AstGrepLang::Java,
            #[cfg(feature = "lang-json")]
            AstGrepLang::Json,
            #[cfg(feature = "lang-lua")]
            AstGrepLang::Lua,
            #[cfg(feature = "lang-nix")]
            AstGrepLang::Nix,
            #[cfg(feature = "lang-php")]
            AstGrepLang::Php,
            #[cfg(feature = "lang-ruby")]
            AstGrepLang::Ruby,
            #[cfg(feature = "lang-scala")]
            AstGrepLang::Scala,
            #[cfg(feature = "lang-solidity")]
            AstGrepLang::Solidity,
            #[cfg(feature = "lang-swift")]
            AstGrepLang::Swift,
            #[cfg(feature = "lang-yaml")]
            AstGrepLang::Yaml,
        ]
    }

    /// Canonical kebab-case name, matching the `--lang` CLI value.
    pub fn name(&self) -> &'static str {
        // `match *self` (not `match self`) so a build with no grammar features,
        // where this enum has zero variants, still type-checks as unreachable.
        match *self {
            #[cfg(feature = "lang-rust")]
            AstGrepLang::Rust => "rust",
            #[cfg(feature = "lang-python")]
            AstGrepLang::Python => "python",
            #[cfg(feature = "lang-typescript")]
            AstGrepLang::TypeScript => "typescript",
            #[cfg(feature = "lang-typescript")]
            AstGrepLang::Tsx => "tsx",
            #[cfg(feature = "lang-javascript")]
            AstGrepLang::JavaScript => "javascript",
            #[cfg(feature = "lang-kotlin")]
            AstGrepLang::Kotlin => "kotlin",
            #[cfg(feature = "lang-bash")]
            AstGrepLang::Bash => "bash",
            #[cfg(feature = "lang-markdown")]
            AstGrepLang::Markdown => "markdown",
            #[cfg(feature = "lang-c")]
            AstGrepLang::C => "c",
            #[cfg(feature = "lang-cpp")]
            AstGrepLang::Cpp => "cpp",
            #[cfg(feature = "lang-csharp")]
            AstGrepLang::CSharp => "csharp",
            #[cfg(feature = "lang-css")]
            AstGrepLang::Css => "css",
            #[cfg(feature = "lang-dart")]
            AstGrepLang::Dart => "dart",
            #[cfg(feature = "lang-elixir")]
            AstGrepLang::Elixir => "elixir",
            #[cfg(feature = "lang-go")]
            AstGrepLang::Go => "go",
            #[cfg(feature = "lang-haskell")]
            AstGrepLang::Haskell => "haskell",
            #[cfg(feature = "lang-hcl")]
            AstGrepLang::Hcl => "hcl",
            #[cfg(feature = "lang-html")]
            AstGrepLang::Html => "html",
            #[cfg(feature = "lang-java")]
            AstGrepLang::Java => "java",
            #[cfg(feature = "lang-json")]
            AstGrepLang::Json => "json",
            #[cfg(feature = "lang-lua")]
            AstGrepLang::Lua => "lua",
            #[cfg(feature = "lang-nix")]
            AstGrepLang::Nix => "nix",
            #[cfg(feature = "lang-php")]
            AstGrepLang::Php => "php",
            #[cfg(feature = "lang-ruby")]
            AstGrepLang::Ruby => "ruby",
            #[cfg(feature = "lang-scala")]
            AstGrepLang::Scala => "scala",
            #[cfg(feature = "lang-solidity")]
            AstGrepLang::Solidity => "solidity",
            #[cfg(feature = "lang-swift")]
            AstGrepLang::Swift => "swift",
            #[cfg(feature = "lang-yaml")]
            AstGrepLang::Yaml => "yaml",
        }
    }

    /// Resolve a user-supplied `--lang` value, accepting common aliases.
    pub fn from_name(name: &str) -> Option<AstGrepLang> {
        let normalized = name.trim().to_ascii_lowercase();
        match normalized.as_str() {
            #[cfg(feature = "lang-rust")]
            "rust" | "rs" => Some(AstGrepLang::Rust),
            #[cfg(feature = "lang-python")]
            "python" | "py" => Some(AstGrepLang::Python),
            #[cfg(feature = "lang-typescript")]
            "typescript" | "ts" => Some(AstGrepLang::TypeScript),
            #[cfg(feature = "lang-typescript")]
            "tsx" => Some(AstGrepLang::Tsx),
            #[cfg(feature = "lang-javascript")]
            "javascript" | "js" | "jsx" => Some(AstGrepLang::JavaScript),
            #[cfg(feature = "lang-kotlin")]
            "kotlin" | "kt" => Some(AstGrepLang::Kotlin),
            #[cfg(feature = "lang-bash")]
            "bash" | "sh" | "shell" => Some(AstGrepLang::Bash),
            #[cfg(feature = "lang-markdown")]
            "markdown" | "md" => Some(AstGrepLang::Markdown),
            // Aliases follow ast-grep's own `impl_aliases!` table so a pattern
            // written against ast-grep docs resolves the same way here.
            #[cfg(feature = "lang-c")]
            "c" => Some(AstGrepLang::C),
            #[cfg(feature = "lang-cpp")]
            "cpp" | "c++" | "cc" | "cxx" => Some(AstGrepLang::Cpp),
            #[cfg(feature = "lang-csharp")]
            "csharp" | "cs" | "c#" => Some(AstGrepLang::CSharp),
            #[cfg(feature = "lang-css")]
            "css" => Some(AstGrepLang::Css),
            #[cfg(feature = "lang-dart")]
            "dart" => Some(AstGrepLang::Dart),
            #[cfg(feature = "lang-elixir")]
            "elixir" | "ex" => Some(AstGrepLang::Elixir),
            #[cfg(feature = "lang-go")]
            "go" | "golang" => Some(AstGrepLang::Go),
            #[cfg(feature = "lang-haskell")]
            "haskell" | "hs" => Some(AstGrepLang::Haskell),
            #[cfg(feature = "lang-hcl")]
            "hcl" | "terraform" | "tf" => Some(AstGrepLang::Hcl),
            #[cfg(feature = "lang-html")]
            "html" | "htm" => Some(AstGrepLang::Html),
            #[cfg(feature = "lang-java")]
            "java" => Some(AstGrepLang::Java),
            #[cfg(feature = "lang-json")]
            "json" => Some(AstGrepLang::Json),
            #[cfg(feature = "lang-lua")]
            "lua" => Some(AstGrepLang::Lua),
            #[cfg(feature = "lang-nix")]
            "nix" => Some(AstGrepLang::Nix),
            #[cfg(feature = "lang-php")]
            "php" => Some(AstGrepLang::Php),
            #[cfg(feature = "lang-ruby")]
            "ruby" | "rb" => Some(AstGrepLang::Ruby),
            #[cfg(feature = "lang-scala")]
            "scala" => Some(AstGrepLang::Scala),
            #[cfg(feature = "lang-solidity")]
            "solidity" | "sol" => Some(AstGrepLang::Solidity),
            #[cfg(feature = "lang-swift")]
            "swift" => Some(AstGrepLang::Swift),
            #[cfg(feature = "lang-yaml")]
            "yaml" | "yml" => Some(AstGrepLang::Yaml),
            _ => None,
        }
    }

    /// Infer the language from a path extension. Returns `None` for files this
    /// build cannot parse, which callers treat as "skip", not "error".
    pub fn from_path(path: &Path) -> Option<AstGrepLang> {
        let ext = path.extension()?.to_str()?.to_ascii_lowercase();
        match ext.as_str() {
            #[cfg(feature = "lang-rust")]
            "rs" => Some(AstGrepLang::Rust),
            #[cfg(feature = "lang-python")]
            "py" | "pyi" => Some(AstGrepLang::Python),
            #[cfg(feature = "lang-typescript")]
            "ts" | "mts" | "cts" => Some(AstGrepLang::TypeScript),
            #[cfg(feature = "lang-typescript")]
            "tsx" => Some(AstGrepLang::Tsx),
            #[cfg(feature = "lang-javascript")]
            "js" | "mjs" | "cjs" | "jsx" => Some(AstGrepLang::JavaScript),
            #[cfg(feature = "lang-kotlin")]
            "kt" | "kts" => Some(AstGrepLang::Kotlin),
            #[cfg(feature = "lang-bash")]
            "sh" | "bash" => Some(AstGrepLang::Bash),
            #[cfg(feature = "lang-markdown")]
            "md" | "markdown" => Some(AstGrepLang::Markdown),
            // `.h` resolves to C, matching ripgrep and tree-sitter convention.
            // C++ headers also use `.h`; pass `--lang cpp` to force that.
            #[cfg(feature = "lang-c")]
            "c" | "h" => Some(AstGrepLang::C),
            #[cfg(feature = "lang-cpp")]
            "cc" | "cpp" | "cxx" | "hpp" | "hh" | "hxx" => Some(AstGrepLang::Cpp),
            #[cfg(feature = "lang-csharp")]
            "cs" => Some(AstGrepLang::CSharp),
            #[cfg(feature = "lang-css")]
            "css" => Some(AstGrepLang::Css),
            #[cfg(feature = "lang-dart")]
            "dart" => Some(AstGrepLang::Dart),
            #[cfg(feature = "lang-elixir")]
            "ex" | "exs" => Some(AstGrepLang::Elixir),
            #[cfg(feature = "lang-go")]
            "go" => Some(AstGrepLang::Go),
            #[cfg(feature = "lang-haskell")]
            "hs" => Some(AstGrepLang::Haskell),
            #[cfg(feature = "lang-hcl")]
            "hcl" | "tf" | "tfvars" => Some(AstGrepLang::Hcl),
            #[cfg(feature = "lang-html")]
            "html" | "htm" => Some(AstGrepLang::Html),
            #[cfg(feature = "lang-java")]
            "java" => Some(AstGrepLang::Java),
            #[cfg(feature = "lang-json")]
            "json" => Some(AstGrepLang::Json),
            #[cfg(feature = "lang-lua")]
            "lua" => Some(AstGrepLang::Lua),
            #[cfg(feature = "lang-nix")]
            "nix" => Some(AstGrepLang::Nix),
            #[cfg(feature = "lang-php")]
            "php" => Some(AstGrepLang::Php),
            #[cfg(feature = "lang-ruby")]
            "rb" => Some(AstGrepLang::Ruby),
            #[cfg(feature = "lang-scala")]
            "scala" | "sc" => Some(AstGrepLang::Scala),
            #[cfg(feature = "lang-solidity")]
            "sol" => Some(AstGrepLang::Solidity),
            #[cfg(feature = "lang-swift")]
            "swift" => Some(AstGrepLang::Swift),
            #[cfg(feature = "lang-yaml")]
            "yaml" | "yml" => Some(AstGrepLang::Yaml),
            _ => None,
        }
    }

    /// Comma-separated list of supported names, for error messages.
    pub fn supported_names() -> String {
        Self::all()
            .iter()
            .map(|lang| lang.name())
            .collect::<Vec<_>>()
            .join(", ")
    }

    #[allow(unused)]
    pub(crate) fn support_lang(&self) -> ast_grep_language::SupportLang {
        use ast_grep_language::SupportLang;
        match *self {
            #[cfg(feature = "lang-rust")]
            AstGrepLang::Rust => SupportLang::Rust,
            #[cfg(feature = "lang-python")]
            AstGrepLang::Python => SupportLang::Python,
            #[cfg(feature = "lang-typescript")]
            AstGrepLang::TypeScript => SupportLang::TypeScript,
            #[cfg(feature = "lang-typescript")]
            AstGrepLang::Tsx => SupportLang::Tsx,
            #[cfg(feature = "lang-javascript")]
            AstGrepLang::JavaScript => SupportLang::JavaScript,
            #[cfg(feature = "lang-kotlin")]
            AstGrepLang::Kotlin => SupportLang::Kotlin,
            #[cfg(feature = "lang-bash")]
            AstGrepLang::Bash => SupportLang::Bash,
            #[cfg(feature = "lang-markdown")]
            AstGrepLang::Markdown => SupportLang::Markdown,
            #[cfg(feature = "lang-c")]
            AstGrepLang::C => SupportLang::C,
            #[cfg(feature = "lang-cpp")]
            AstGrepLang::Cpp => SupportLang::Cpp,
            #[cfg(feature = "lang-csharp")]
            AstGrepLang::CSharp => SupportLang::CSharp,
            #[cfg(feature = "lang-css")]
            AstGrepLang::Css => SupportLang::Css,
            #[cfg(feature = "lang-dart")]
            AstGrepLang::Dart => SupportLang::Dart,
            #[cfg(feature = "lang-elixir")]
            AstGrepLang::Elixir => SupportLang::Elixir,
            #[cfg(feature = "lang-go")]
            AstGrepLang::Go => SupportLang::Go,
            #[cfg(feature = "lang-haskell")]
            AstGrepLang::Haskell => SupportLang::Haskell,
            #[cfg(feature = "lang-hcl")]
            AstGrepLang::Hcl => SupportLang::Hcl,
            #[cfg(feature = "lang-html")]
            AstGrepLang::Html => SupportLang::Html,
            #[cfg(feature = "lang-java")]
            AstGrepLang::Java => SupportLang::Java,
            #[cfg(feature = "lang-json")]
            AstGrepLang::Json => SupportLang::Json,
            #[cfg(feature = "lang-lua")]
            AstGrepLang::Lua => SupportLang::Lua,
            #[cfg(feature = "lang-nix")]
            AstGrepLang::Nix => SupportLang::Nix,
            #[cfg(feature = "lang-php")]
            AstGrepLang::Php => SupportLang::Php,
            #[cfg(feature = "lang-ruby")]
            AstGrepLang::Ruby => SupportLang::Ruby,
            #[cfg(feature = "lang-scala")]
            AstGrepLang::Scala => SupportLang::Scala,
            #[cfg(feature = "lang-solidity")]
            AstGrepLang::Solidity => SupportLang::Solidity,
            #[cfg(feature = "lang-swift")]
            AstGrepLang::Swift => SupportLang::Swift,
            #[cfg(feature = "lang-yaml")]
            AstGrepLang::Yaml => SupportLang::Yaml,
        }
    }
}

impl std::fmt::Display for AstGrepLang {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.name())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolves_names_and_aliases() {
        #[cfg(feature = "lang-rust")]
        {
            assert_eq!(AstGrepLang::from_name("rust"), Some(AstGrepLang::Rust));
            assert_eq!(AstGrepLang::from_name("  RS "), Some(AstGrepLang::Rust));
        }
        assert_eq!(AstGrepLang::from_name("cobol"), None);
    }

    #[test]
    fn infers_language_from_extension() {
        #[cfg(feature = "lang-rust")]
        assert_eq!(
            AstGrepLang::from_path(Path::new("a/b/c.rs")),
            Some(AstGrepLang::Rust)
        );
        assert_eq!(AstGrepLang::from_path(Path::new("a/b/c.bin")), None);
        assert_eq!(AstGrepLang::from_path(Path::new("noext")), None);
    }

    #[test]
    fn every_listed_language_round_trips_through_its_name() {
        // Guards the three parallel `match` arms in this module against drift:
        // a language added to `all()` but forgotten in `from_name` would ship a
        // name the CLI advertises but cannot resolve.
        let langs = AstGrepLang::all();
        assert!(!langs.is_empty(), "no grammars compiled into this build");
        for lang in langs {
            assert_eq!(
                AstGrepLang::from_name(lang.name()),
                Some(*lang),
                "language {lang} is listed but its own name does not resolve"
            );
        }
    }

    #[test]
    fn every_listed_language_is_reachable_from_some_file_extension() {
        // `from_path` is the fourth parallel match arm and the one the
        // round-trip test above cannot reach: a language added to `all()` and
        // `from_name` but forgotten in `from_path` would be advertised and
        // `--lang`-selectable while every directory walk silently skipped its
        // files. Extensions are probed rather than listed so this stays a drift
        // guard and not a second copy of the table.
        const CANDIDATES: &[&str] = &[
            "rs", "py", "ts", "tsx", "js", "kt", "sh", "md", "c", "cc", "cs", "css", "dart", "ex",
            "go", "hs", "hcl", "html", "java", "json", "lua", "nix", "php", "rb", "scala", "sol",
            "swift", "yaml",
        ];
        for lang in AstGrepLang::all() {
            let reachable = CANDIDATES.iter().any(|ext| {
                AstGrepLang::from_path(Path::new(&format!("probe.{ext}"))) == Some(*lang)
            });
            assert!(
                reachable,
                "language {lang} is listed but no file extension resolves to it"
            );
        }
    }

    #[test]
    fn every_listed_language_maps_to_a_compiled_support_lang() {
        // `support_lang()` is what actually hands a parser to the engine. A
        // variant present here but mismapped there is a wrong-grammar parse,
        // which is worse than a refusal because it silently under-matches.
        for lang in AstGrepLang::all() {
            let support = lang.support_lang();
            assert_eq!(
                support.to_string().to_ascii_lowercase().replace(['-', '#'], ""),
                lang.name().replace('-', ""),
                "language {lang} maps to ast-grep SupportLang::{support:?}"
            );
        }
    }
}

/// Grammar-level pattern quirks that are properties of the upstream tree-sitter
/// grammars, not of tsift. They are pinned here so the documented workarounds in
/// `specs/structural-patterns.md` cannot silently drift with an ast-grep bump.
#[cfg(test)]
mod grammar_quirk_tests {
    use super::*;
    use crate::search_source;

    #[cfg(feature = "lang-c")]
    #[test]
    fn c_call_patterns_need_a_trailing_semicolon() {
        // `foo($A)` as a standalone C pattern is ambiguous: tree-sitter-c reads
        // it as a declaration (`foo` a type, `$A` a declarator), not a call.
        // The trailing `;` forces the expression-statement reading.
        const SRC: &str = "int main(void) {\n  foo(1);\n  foo(2);\n  return 0;\n}\n";
        assert_eq!(
            search_source(SRC, AstGrepLang::C, "foo($A)").unwrap().len(),
            0,
            "if this starts matching, the C workaround note is stale"
        );
        assert_eq!(
            search_source(SRC, AstGrepLang::C, "foo($A);").unwrap().len(),
            2
        );
    }

    #[cfg(feature = "lang-css")]
    #[test]
    fn css_declaration_patterns_need_a_trailing_semicolon() {
        const SRC: &str = "body { color: red; }\n.x { color: blue; }\n";
        assert_eq!(
            search_source(SRC, AstGrepLang::Css, "color: $V")
                .unwrap()
                .len(),
            0
        );
        assert_eq!(
            search_source(SRC, AstGrepLang::Css, "color: $V;")
                .unwrap()
                .len(),
            2
        );
    }

    #[cfg(feature = "lang-dart")]
    #[test]
    fn dart_matches_only_at_declaration_granularity() {
        // tree-sitter-dart cannot parse a bare expression as a standalone
        // pattern, so call-shaped patterns find nothing however they are
        // spelled. Whole declarations, including metavariables inside them,
        // work. This is the sharpest limit among the structural-only languages.
        const SRC: &str = "void main() {\n  print(\"a\");\n}\n";
        for pattern in ["print($A)", "print($A);", "print(\"a\")", "foo(1)"] {
            assert_eq!(
                search_source(SRC, AstGrepLang::Dart, pattern).unwrap().len(),
                0,
                "dart pattern {pattern:?} unexpectedly matched — update the known-limits note"
            );
        }
        assert_eq!(
            search_source(SRC, AstGrepLang::Dart, "void main() { print($A); }")
                .unwrap()
                .len(),
            1
        );
    }

    #[cfg(feature = "lang-solidity")]
    #[test]
    fn solidity_matches_only_at_declaration_granularity() {
        const SRC: &str = "contract C {\n  function f() public { emit E(1); }\n}\n";
        for pattern in ["emit E($A)", "emit E($A);", "E($A)"] {
            assert_eq!(
                search_source(SRC, AstGrepLang::Solidity, pattern)
                    .unwrap()
                    .len(),
                0,
                "solidity pattern {pattern:?} unexpectedly matched — update the known-limits note"
            );
        }
        assert_eq!(
            search_source(SRC, AstGrepLang::Solidity, "function f() public { $$$B }")
                .unwrap()
                .len(),
            1
        );
    }

    #[cfg(all(feature = "lang-go", feature = "lang-java", feature = "lang-cpp"))]
    #[test]
    fn the_priority_codemod_languages_match_call_expressions_directly() {
        // go / cpp / java are the languages this tier was added for. If any of
        // them regressed to dart-like declaration-only matching, cross-repo
        // codemods would stop working and no test above would notice.
        assert_eq!(
            search_source(
                "package main\n\nfunc main() {\n\tfoo(1)\n\tfoo(2)\n}\n",
                AstGrepLang::Go,
                "foo($A)"
            )
            .unwrap()
            .len(),
            2
        );
        assert_eq!(
            search_source(
                "int main() {\n  foo(1);\n  foo(2);\n}\n",
                AstGrepLang::Cpp,
                "foo($A)"
            )
            .unwrap()
            .len(),
            2
        );
        assert_eq!(
            search_source(
                "class A {\n  void r() { log.debug(\"x\"); log.debug(\"y\"); }\n}\n",
                AstGrepLang::Java,
                "log.debug($A)"
            )
            .unwrap()
            .len(),
            2
        );
    }
}
