pub mod prompt_cache_history;
pub mod session_cost;
pub mod session_digest;
pub mod session_markdown;
pub mod session_review;

// Note (#kgaduse): `tsift-agent-doc` previously re-exported `tsift_kg as kg`
// without any call site. The README contract is the opposite — agent-doc may
// READ `.tsift/graph.db` evidence via the tsift-sqlite/tsift-core GraphStore
// layer, but must not own or expose the tsift-kg extraction pipeline. The
// dead dep was removed; the real consumer (read-only graph evidence lookup
// for planning/orchestration) is tracked as #kgadactivate.
