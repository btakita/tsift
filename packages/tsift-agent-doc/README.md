# tsift-agent-doc

Agent-doc and session observability helpers for tsift.

This crate provides transcript cost, digest, review, guardrail, and log parsing
surfaces used by the `tsift` CLI. It is part of the versioned tsift workspace and
is published with the rest of the split crates.

## Local KG Model Contract

Agent-doc consumes local Knowledge Graph evidence through tsift-owned graph
stores only. The contract lives in
[`../../specs/local-kg-model.md`](../../specs/local-kg-model.md): `tsift-agent-doc`
may read `.tsift/graph.db` evidence, run manifests, and provenance rows, but it
must not own a separate local-model provider, prompt schema, or VRAM lifecycle.

When agent-doc planning/orchestration needs semantic KG evidence, it should call
the `tsift` CLI or a `tsift-*` library boundary that returns validated
`GraphProjection`/`GraphStore` data.

### Current state

`graph_evidence` (added in `#kgadactivate`) is the read seam: a typed,
read-only `SqliteGraphStore::open_read_only_resilient` lookup that returns
`GraphEvidenceReport` snapshots for a symbol/kind query. The
`tsift kg evidence` CLI exposes it; library callers
(`session_digest`/planning) are tracked as a separate design pass.

Prior state: the dropped `pub use tsift_kg as kg;` re-export (`#kgaduse`)
was a dead dep with no call site and the wrong layer (extraction, not
store-read). It was removed before this seam landed.
