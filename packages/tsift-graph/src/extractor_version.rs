//! Extractor versions stamped on every indexed file (`#tsiftindexesinvalidate`).
//!
//! `tsift index` skips a file whose mtime matches its `file_state` row. That is
//! only sound while the code that produced the file's rows is the code that
//! would run now. An extractor change (new symbol kinds, a call-site or route
//! query fix) leaves every unchanged file holding rows from the old extractor,
//! so after `daac4fe` a plain `tsift index` kept indexes without TypeScript
//! methods, C# using-aliases, and C++ namespace variables until `--rebuild`.
//!
//! Each `file_state` row records the version string returned by
//! [`extractor_version`] for its language. A stored version that differs from
//! the running binary's makes the file `modified`, so the next plain
//! `tsift index` re-extracts exactly the files whose extractor changed.
//!
//! When to bump:
//! - Changed what one language extracts (symbols, call sites, routes, spans):
//!   bump that language's entry in [`LANGUAGE_EXTRACTOR_VERSIONS`]. Only files
//!   of that language re-extract. Languages sharing one extractor
//!   (`typescript`/`tsx`, `javascript`/`jsx`) are bumped together.
//! - Changed something every language goes through (shared symbol/edge
//!   resolution, the zone-map, FTS rows, the stored columns): bump
//!   [`EXTRACTOR_SCHEMA_VERSION`]. Every file re-extracts.
//! - Added a language: nothing is required (its files are new to the index);
//!   add an entry when its extractor first changes, or leave it at the default.

/// Version of the per-file rows every language shares. Bumping it re-extracts
/// every file in every index on the next `tsift index`.
pub const EXTRACTOR_SCHEMA_VERSION: u32 = 1;

/// Version a language gets when it has no entry in
/// [`LANGUAGE_EXTRACTOR_VERSIONS`].
pub const DEFAULT_LANGUAGE_EXTRACTOR_VERSION: u32 = 1;

/// Per-language extractor versions, keyed by `Lang::name()`. Bump an entry
/// whenever that language's symbol, call-site, or route extraction changes.
pub const LANGUAGE_EXTRACTOR_VERSIONS: &[(&str, u32)] = &[
    ("bash", 1),
    ("c", 1),
    ("cpp", 1),
    ("csharp", 1),
    ("gdscript", 1),
    ("go", 1),
    ("jai", 1),
    ("javascript", 1),
    ("json", 1),
    ("jsx", 1),
    ("kotlin", 1),
    ("luau", 1),
    ("markdown", 1),
    ("odin", 1),
    ("python", 1),
    ("rust", 1),
    ("tsx", 1),
    ("typescript", 1),
    ("wsdl", 1),
    ("xsd", 1),
    ("yaml", 1),
    ("zig", 1),
];

/// The per-language component of [`extractor_version`].
pub fn language_extractor_version(language: &str) -> u32 {
    LANGUAGE_EXTRACTOR_VERSIONS
        .iter()
        .find(|(name, _)| *name == language)
        .map(|(_, version)| *version)
        .unwrap_or(DEFAULT_LANGUAGE_EXTRACTOR_VERSION)
}

/// The version string stored in `file_state.extractor_version` for a file of
/// `language`: `"<schema>.<language version>"`. Compared for equality only.
pub fn extractor_version(language: &str) -> String {
    format!(
        "{EXTRACTOR_SCHEMA_VERSION}.{}",
        language_extractor_version(language)
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn language_table_has_no_duplicate_names() {
        let mut names: Vec<&str> = LANGUAGE_EXTRACTOR_VERSIONS
            .iter()
            .map(|(name, _)| *name)
            .collect();
        names.sort_unstable();
        let before = names.len();
        names.dedup();
        assert_eq!(before, names.len(), "duplicate language in extractor table");
    }

    #[test]
    fn shared_extractors_carry_the_same_version() {
        assert_eq!(
            language_extractor_version("typescript"),
            language_extractor_version("tsx")
        );
        assert_eq!(
            language_extractor_version("javascript"),
            language_extractor_version("jsx")
        );
    }

    #[test]
    fn version_string_includes_schema_and_language() {
        assert_eq!(
            extractor_version("rust"),
            format!(
                "{EXTRACTOR_SCHEMA_VERSION}.{}",
                language_extractor_version("rust")
            )
        );
        assert_eq!(
            extractor_version("not-a-language"),
            format!("{EXTRACTOR_SCHEMA_VERSION}.{DEFAULT_LANGUAGE_EXTRACTOR_VERSION}")
        );
    }
}
