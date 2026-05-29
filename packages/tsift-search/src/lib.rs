//! `tsift-search` — search ranking, impact analysis, and tagpath annotation
//! surfaces extracted from root `tsift`.
//!
//! Root `tsift` re-exports these modules via
//! `pub use tsift_search::{impact, sift, tagpath_adapter};` so existing
//! `tsift::sift::*` / `tsift::impact::*` / `tsift::tagpath_adapter::*` callers
//! keep compiling.

pub mod impact;
pub mod sift;
pub mod tagpath_adapter;
