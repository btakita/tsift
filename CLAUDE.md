# tsift

Token-efficient CLI for code agents — AST-aware search, call-graph queries, batch editing, SQL introspection, and model routing.

## Architecture

Single-binary Rust CLI (`src/main.rs`). All commands are subcommands via clap derive:

| Command | Purpose |
|---------|---------|
| `tsift index` | Build AST symbol index via tree-sitter. `--workspace` / `--submodule <name>` / `--prune` / `--check` (dry-run) / `--exit-code` (exit 1 if stale, for hooks) |
| `tsift search` | Hybrid BM25 + vector search via sift library. `--federated` / `--scope <name>` for workspace |
| `tsift graph` | Call-graph queries: `--callers` / `--callees` of a symbol. `--limit N` (default 20, 0=unlimited) / `--scope <name>` / `--json` |
| `tsift edit` | Batch file edits from JSON (stdin or `--file`), atomic validate-then-write |
| `tsift route` | Classify task → model tier (haiku/sonnet/opus) |
| `tsift rewrite` | Shell command → tsift equivalent (for Claude Code hook integration) |
| `tsift sql` | SQLite introspection: schema overview, table detail, read-only query |
| `tsift communities` | Louvain community detection over call graph. `--min-size N` / `--limit N` (default 10, 0=unlimited) / `--scope <name>` / `--json` |
| `tsift path` | BFS shortest path between two symbols. `--scope <name>` / `--json` |
| `tsift explain` | Full symbol context: definitions, callers, callees, community. `--limit N` (default 15, 0=unlimited) / `--scope <name>` / `--json` |
| `tsift audit` | Skill drift detection: scan installed skills, check health, compare against manifest, detect duplicates via Jaccard similarity. `--manifest <file>` / `--usage` / `--cleanup` / `--report <path>` / `--json` |
| `tsift summarize` | Cached LLM analysis: pre-computed summaries, entities, relationships. `--extract <path>` / `--extract --diff` / `--file <path>` / `--stats` / `--json` |
| `tsift lint` | Markdown lint: detect unannotated concepts (symbols, headings, bold terms) cross-referenced against graph entities. `--index <dir>` / `--entities-from <file>` / `--json` |
| `tsift status` | Session health check: index freshness, summary cache, recommended commands. `--json` for structured output |
| `tsift init` | Project setup: ensure Code Navigation section in AGENTS.md and mirror it into CLAUDE.md when present. Idempotent — safe to re-run after upgrades. |

Global flags: `--compact` reduces human-readable output volume. `--pretty` switches JSON output from compact (default) to indented format. `--terse` outputs JSON with abbreviated field names and inline schema (implies `--json`). `--absolute` shows full filesystem paths instead of project-relative (relative is default for token savings).

## Graph Module (`src/graph.rs`)

Call-graph extraction via tree-sitter. Runs during `tsift index` and stores edges in `call_edges` table.

- `extract_call_sites(lang, source)` — parse source, find all function/method/macro calls
- `resolve_edges(symbols, call_sites)` — match call sites to enclosing functions (innermost wins)
- Supported: Rust (direct, method, scoped, macros), Python, TypeScript/TSX, JavaScript/JSX, Kotlin
- Skipped: Zig, Bash, Markdown (no meaningful call patterns)
- `detect_communities(edges)` — Louvain phase 1 modularity optimization, returns communities sorted by size
- `shortest_path(edges, from, to)` — BFS over undirected call graph, returns path + hop count

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
make check          # clippy + test (236 tests)
cargo install --path .   # install to ~/.cargo/bin/
```

## Conventions

- **Read-only SQL**: `open_db()` uses `SQLITE_OPEN_READ_ONLY` — never mutates user databases.
- **Atomic batch edits**: `cmd_edit` validates ALL edits before writing ANY (two-phase).
- **Rewrite protocol**: exit 0 + stdout = rewrite, exit 1 = pass through (matches rtk convention).
- **Search strategy default**: `lexical` for instant results. `hybrid`/`vector` require model download on first run.
- **All public logic tested**: `classify_task`, `apply_edit_op`, `rewrite_command`, SQL helpers all have unit tests.

## Hook Integration

- **Auto-reindex** (`UserPromptSubmit`): `examples/hooks/tsift-autoindex.sh` runs `tsift index --check --exit-code .` on every prompt, auto-reindexes when stale. Install via `.claude/settings.json`.
- **Search rewrite** (`PreToolUse`): `~/.claude/hooks/tsift-rewrite.sh` rewrites `rg`/`grep -r` to `tsift search --strategy lexical`.
- **RTK output filtering** (`PreToolUse`): same hook routes verbose commands (`communities`, `explain`, `graph`, `index`, `search`) through RTK when installed. TOML filters at `~/.config/rtk/filters.toml` cap output lines.

## Repo

Private: `github.com/btakita/tsift`. Submodule at `src/tsift` in agent-loop.

<!-- tsift:code-navigation -->
## Code Navigation

Run `tsift status` at session start. Use the commands listed in its `use:` output:
- `tsift search <query>` — AST-aware hybrid search (prefer over grep/rg)
- `tsift explain <symbol>` — callers, callees, community context
- `tsift graph <symbol> --callers` / `--callees` — call graph navigation
- `tsift summarize <symbol>` — cached summary (only when listed in `use:`)

Only read full source files when tsift results are insufficient.
<!-- /tsift:code-navigation -->
