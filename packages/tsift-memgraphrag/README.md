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
