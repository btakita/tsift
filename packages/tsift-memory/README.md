# tsift-memory

First-party memory substrate for tsift and agent-doc.

This crate owns the stable memory schema, budgeted capture contracts, and migration importers. It treats external memory tools such as `claude-mem` as import sources instead of durable runtime dependencies.

Current surfaces:

- `MemoryStore` initializes `.tsift/memory.db` with schema version `1`.
- `plan_capture_handoff` estimates tokens before model calls and reports split/defer decisions.
- `guard_memory_handoff` fails closed on oversized raw payloads and emits digest/context replacements plus retryable chunk commands.
- `inspect_claude_mem` and `read_claude_mem_events` inspect the observed `claude-mem` SQLite schema without writing to it.
- `import_claude_mem` optionally migrates those events into the tsift memory DB; uncapped imports report per-table source/read/import reconciliation and capped event-id samples so operators can prove all supported rows were covered without oversized JSON.

Graph/RAG behavior lives in `tsift-memgraphrag`, which depends on this crate for
events and imports. `tsift-memory` does not depend back on MemGraphRAG.
