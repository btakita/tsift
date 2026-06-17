# tsift-graph

Language-aware graph extraction for tsift.

This crate owns tree-sitter symbol and call extraction, route edges, community
helpers, and path finding used by the tsift index and graph surfaces. It is part
of the versioned tsift workspace and is published with the rest of the split
crates.

## Local KG Model Contract

`tsift-graph` remains the deterministic structural graph extractor. Local
LLM-backed KG facts are specified in
[`../../specs/local-kg-model.md`](../../specs/local-kg-model.md) and must enter
the graph through provider-neutral `GraphProjection` rows with stable ids,
provenance, freshness, and content hashes. The local KG path must not replace
tree-sitter symbol/call extraction; it augments the shared graph with semantic
entity and relation facts.
