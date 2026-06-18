pub mod graph_evidence;
pub mod prompt_cache_history;
pub mod session_cost;
pub mod session_digest;
pub mod session_markdown;
pub mod session_review;

// Local KG Model Contract (#kgadactivate, replacing the dropped #kgaduse
// re-export): agent-doc may READ `.tsift/graph.db` evidence via the
// tsift-sqlite/tsift-core GraphStore layer but must not own the tsift-kg
// extraction pipeline. `graph_evidence` is that read seam. Deeper wiring
// into session_digest/planning call sites is a separate design pass.
//
// See specs/local-kg-model.md lines 29-30 and the README's Local KG Model
// Contract section for the contract this module realizes.
