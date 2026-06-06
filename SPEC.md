# Spec: AST-Aware Code RAG

## Goal

Extend tsift with tree-sitter AST parsing, dependency graph tracking, and per-submodule isolation to enable token-efficient code retrieval at the function/symbol level rather than file level.

## Spec Map

This file is the stable index. Normative detail lives in behavior-bounded sibling specs under [`specs/`](specs/). Find the right file by topic:

| Spec file | Owns |
|-----------|------|
| [specs/architecture.md](specs/architecture.md) | Architecture, Per-Submodule Isolation, Multiplicity Model, Storage Layout, Multi-Language Architecture, Development Phases, Key Design Decision: Graph > Vector for Code |
| [specs/graph.md](specs/graph.md) | Provider-Neutral Graph Substrate (Graph DB Performance Release Gate, Cross-Surface Token Gate, Cycle Packet Cache, libsql Backend, SurrealDB Backend Spike), Findings Graph Layer, Graph Traversal Handles (lazily-rs Cache Contracts), Semantic Graph Similarity, Community Detection, Graph Path Queries, Graph CLI End-to-End Coverage |
| [specs/search-navigation.md](specs/search-navigation.md) | New Subcommands, Search Stale Precheck + Timeout, Bounded Source-File Reads, Handle-Preserving Search Workflow, Index Quiet Mode, Init, Code Navigation, Status, Summarize |
| [specs/output-formats.md](specs/output-formats.md) | Global Compact Output, Budget-Aware Preview Profiles, Structured Envelopes, Compact/Terse/Ultra-Terse JSON, Tabular Output, Schema-Then-Values Mode, Relative Paths, Output Caps, Output Contract |
| [specs/digests-sessions.md](specs/digests-sessions.md) | Diff Digest, Test Digest, Metric Digest, DCI Benchmark, Deterministic SimWorld, Log Digest, Session Digest, Session Cost, Session Review |
| [specs/release-integration.md](specs/release-integration.md) | Release Workflow, Skill Audit, Markdown Lint, Hook Integration, Tagpath integration, What NOT to build |

**Authoring rule:** when a section grows or a new behavior boundary appears, follow the agent-doc `split-spec-files` runbook — keep this index stable and move normative detail into the sibling that owns that boundary. Do not duplicate an invariant across siblings unless intentional and kept in sync.
