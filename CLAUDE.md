# tsift

Token-efficient CLI plugin for Claude Code — AST-aware search, call-graph queries, batch editing, SQL introspection, and model routing.

## Architecture

Single-binary Rust CLI (`src/main.rs`). All commands are subcommands via clap derive:

| Command | Purpose |
|---------|---------|
| `tsift index` | Build AST symbol index via tree-sitter. `--workspace` / `--submodule <name>` / `--prune` (dir mtime pruning for large repos) |
| `tsift search` | Hybrid BM25 + vector search via sift library. `--federated` / `--scope <name>` for workspace |
| `tsift graph` | Call-graph queries: `--callers` / `--callees` of a symbol. `--scope <name>` / `--json` |
| `tsift edit` | Batch file edits from JSON (stdin or `--file`), atomic validate-then-write |
| `tsift route` | Classify task → model tier (haiku/sonnet/opus) |
| `tsift rewrite` | Shell command → tsift equivalent (for Claude Code hook integration) |
| `tsift sql` | SQLite introspection: schema overview, table detail, read-only query |

## Graph Module (`src/graph.rs`)

Call-graph extraction via tree-sitter. Runs during `tsift index` and stores edges in `call_edges` table.

- `extract_call_sites(lang, source)` — parse source, find all function/method/macro calls
- `resolve_edges(symbols, call_sites)` — match call sites to enclosing functions (innermost wins)
- Supported: Rust (direct, method, scoped, macros), Python, TypeScript/TSX, JavaScript/JSX, Kotlin
- Skipped: Zig, Bash, Markdown (no meaningful call patterns)

**Query via CLI:**
```bash
tsift graph <symbol> --callers    # who calls this?
tsift graph <symbol> --callees    # what does this call?
tsift graph <symbol>              # both directions
tsift graph <symbol> --json       # structured output
tsift graph <symbol> --scope sub  # restrict to submodule
```

## Dependencies

- **sift** — git dep (`github.com/rupurt/sift`), not on crates.io. Provides `Sift::builder().build()` + `engine.search()`.
- **rusqlite** — `bundled` feature (no system SQLite needed).
- **tree-sitter** + per-language grammar crates — AST parsing for symbol extraction and call-graph.
- **clap**, **anyhow**, **serde**, **serde_json** — standard Rust CLI stack.

## Development

```bash
make check          # clippy + test (131 tests)
cargo install --path .   # install to ~/.cargo/bin/
```

## Conventions

- **Read-only SQL**: `open_db()` uses `SQLITE_OPEN_READ_ONLY` — never mutates user databases.
- **Atomic batch edits**: `cmd_edit` validates ALL edits before writing ANY (two-phase).
- **Rewrite protocol**: exit 0 + stdout = rewrite, exit 1 = pass through (matches rtk convention).
- **Search strategy default**: `lexical` for instant results. `hybrid`/`vector` require model download on first run.
- **All public logic tested**: `classify_task`, `apply_edit_op`, `rewrite_command`, SQL helpers all have unit tests.

## Hook Integration

`tsift rewrite` is wired as a Claude Code PreToolUse hook via `~/.claude/hooks/tsift-rewrite.sh`. Rewrites `rg`/`grep -r` commands to `tsift search --strategy lexical`.

## Repo

Private: `github.com/btakita/tsift`. Submodule at `src/tsift` in agent-loop.
