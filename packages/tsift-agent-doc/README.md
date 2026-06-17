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
