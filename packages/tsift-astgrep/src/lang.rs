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
}
