# tsift-memory

First-party memory substrate for tsift and agent-doc.

This crate owns the stable memory schema, budgeted capture contracts, graph projection primitives, and migration importers. It treats external memory tools such as `claude-mem` as import sources instead of durable runtime dependencies.

Current surfaces:

- `MemoryStore` initializes `.tsift/memory.db` with schema version `1`.
- `plan_capture_handoff` estimates tokens before model calls and reports split/defer decisions.
- `guard_memory_handoff` fails closed on oversized raw payloads and emits digest/context replacements plus retryable chunk commands.
- `project_memory_events` maps memory events into provider-neutral graph nodes/edges.
- `inspect_claude_mem` and `import_claude_mem` read the observed `claude-mem` SQLite schema and optionally import events into the tsift memory DB.
