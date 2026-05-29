//! `tsift-status` — session health + lock diagnostics extracted from root `tsift`.
//!
//! Backs `tsift status` and `tsift locks`: index freshness, instruction-version
//! checks, summary-cache recovery diagnostics, and lock-sidecar / journal state.
//! Root `tsift` re-exports via `pub use tsift_status::status;` so existing
//! `tsift::status::*` callers keep compiling.

pub mod status;
