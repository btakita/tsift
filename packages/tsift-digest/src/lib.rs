//! `tsift-digest` — code-aware digest emitters extracted from root `tsift`.
//!
//! High-cohesion emitter group with shared serialization, schema versioning,
//! and summary-cache enrichment. Root `tsift` re-exports these modules via
//! `pub use tsift_digest::{diff_digest, log_digest, metric_digest, test_digest};`
//! so existing `tsift::diff_digest::*` callers keep compiling.

pub mod diff_digest;
pub mod log_digest;
pub mod metric_digest;
pub mod test_digest;
