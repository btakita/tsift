//! Structural code search and rewrite for tsift, backed by [ast-grep].
//!
//! tsift's existing `search` is index-backed and token-oriented: it answers
//! "where is this concept". This crate answers the complementary question —
//! "where does this *shape* of code occur, and rewrite it" — using ast-grep
//! patterns (`foo($A)`, `if ($C) { $$$BODY }`) over the same tree-sitter
//! grammars tsift already compiles.
//!
//! [ast-grep]: https://ast-grep.github.io
//!
//! ```no_run
//! use tsift_astgrep::{AstGrepLang, scan, ScanOptions};
//! let report = scan("unwrap()", &ScanOptions::default())?;
//! println!("{} matches", report.match_count);
//! # Ok::<(), anyhow::Error>(())
//! ```

mod engine;
mod lang;
mod scan;

pub use engine::{RewriteOutcome, StructuralMatch, rewrite_source, search_source};
pub use lang::AstGrepLang;
pub use scan::{
    FileMatches, FileRewrite, RewriteReport, ScanOptions, ScanReport, codemod, scan,
};
