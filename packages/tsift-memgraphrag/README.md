# tsift-memgraphrag

MemGraphRAG graph projection and retrieval layer for tsift.

This crate owns the graph/RAG side of first-party memory:

- decay-weighted memory ranking and query packet contracts,
- projection of `tsift-memory` events into `GraphProjection` rows with explicit
  recent-first, oldest-first, and query-relevant read policies,
- upsert of `.tsift/memory.db` into the shared graph store,
- memory projection metadata nodes carrying read policy, source watermark, and
  content hash proof,
- traversal refresh rows for memory/session/source/semantic retrieval,
- ontology derivation over the shared graph store.

`tsift-memory` remains the durable event database and import substrate. This
crate depends on it; `tsift-memory` does not depend back on MemGraphRAG.

## Local KG Model Contract

`tsift-memgraphrag` consumes the shared local KG contract from
[`../../specs/local-kg-model.md`](../../specs/local-kg-model.md). It must keep
`tsift-local-hash-v1` as the deterministic fallback, but semantic extraction and
embedding providers should plug in through `tsift-kg`/`tsift-local-model`
contracts instead of hard-coding a model runtime in this crate.

Projected KG facts must remain normal `GraphProjection` nodes and edges with
`tsift-kg` provenance, source watermarks, content hashes, and run manifest
references so repeated local model runs can be compared without duplicating
unchanged graph facts.
