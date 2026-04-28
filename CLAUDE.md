# tsift

Token-efficient CLI for code agents — AST-aware search, call-graph queries, batch editing, SQL introspection, and model routing.

## Architecture

Single-binary Rust CLI (`src/main.rs`). All commands are subcommands via clap derive:

| Command | Purpose |
|---------|---------|
| `tsift index` | Build AST symbol index via tree-sitter. `--workspace` / `--submodule <name>` / `--prune` (currently a conservative full scan for correctness) / `--check` (dry-run) / `--exit-code` (exit 1 if stale, for hooks) |
| `tsift search` | Hybrid BM25 + vector search via sift library. Built-in stale precheck + optional `--autoindex`. `--federated` / `--scope <name>` for workspace; unknown scopes fail closed with the available submodule names |
| `tsift graph` | Call-graph queries: `--callers` / `--callees` of a symbol. `--limit N` (default 20, 0=unlimited) / `--scope <name>` / `--json` |
| `tsift edit` | Batch file edits from JSON (stdin or `--file`), atomic validate-then-write |
| `tsift route` | Classify task → model tier (haiku/sonnet/opus) |
| `tsift rewrite` | Shell command → tsift equivalent (for Claude Code hook integration) |
| `tsift sql` | SQLite introspection: schema overview, table detail, read-only query |
| `tsift communities` | Louvain community detection over call graph. `--min-size N` / `--limit N` (default 10, 0=unlimited) / `--scope <name>` / `--json` |
| `tsift path` | BFS shortest path between two symbols. `--scope <name>` / `--json` |
| `tsift explain` | Full symbol context: definitions, callers, callees, community. `--limit N` (default 15, 0=unlimited) / `--scope <name>` / `--json` |
| `tsift audit` | Skill drift detection: scan installed skills, check health, compare against manifest, detect duplicates via Jaccard similarity. `--manifest <file>` / `--usage` / `--cleanup` / `--report <path>` / `--json` |
| `tsift summarize` | Cached LLM analysis: pre-computed summaries, entities, relationships. `--extract <path>` / `--extract --diff` (scoped to the requested file/dir; per-file cache rewrite is transactional; non-2xx Anthropic responses fail closed with status + API message) / `--file <path>` / `--stats` / `--json` |
| `tsift lint` | Markdown lint: detect unannotated concepts (symbols, headings, bold terms) cross-referenced against graph entities. Auto-discovers live `index.db` files from the nearest `.tsift` root by default. `--index <dir>` / `--entities-from <file>` / `--json` |
| `tsift status` | Session health check: index freshness, instruction version, summary cache, recommended commands. `--json` for structured output |
| `tsift locks` | Diagnose the OS-backed `index.lock` sidecar and `index.db-journal` state, and recommend the next recovery step. Stale sidecar metadata is reused automatically. `--scope <name>` / `--json` |
| `tsift init` | Project setup: ensure versioned Code Navigation section (`v=X.Y.Z`) in AGENTS.md and mirror it into CLAUDE.md when present. `--codex` injects or updates a repo-aware autoindex hook; `--workspace` resolves to the parent workspace root. Detects and refreshes stale/pre-versioned markers on re-run. |

Global flags: `--compact` reduces human-readable output volume (abbreviated kind/match_type labels, shorter section headers like `syms`, `crs`, `ces`, `comm`). `--pretty` switches JSON output from compact (default) to indented format. `--terse` outputs JSON with abbreviated field names and inline schema (implies `--json`). `--schema` converts repeated object arrays to columnar `{"_c":[cols],"_r":[[vals],...]}` format (implies `--json`; combines with `--terse`). `--absolute` shows full filesystem paths instead of project-relative (relative is default for token savings). `--tabular` outputs repeated structures as TSV with header row (search, graph, communities, explain).

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
make check          # clippy + full test suite
cargo install --path .   # install to ~/.cargo/bin/
```

## Versioning

Canonical version source: [`Cargo.toml`](Cargo.toml) `package.version`. The installed binary exposes the same value via `tsift --version`.

Change history lives in [`VERSIONS.md`](VERSIONS.md). Even while tsift remains private, keep the Cargo package version and a matching `VERSIONS.md` entry in sync when the command surface or behavior changes.

If copied skill instructions lag behind the installed binary, treat this file, `VERSIONS.md`, and `tsift --help` as the current source of truth.

## Conventions

- **Read-only SQL**: `open_db()` uses `SQLITE_OPEN_READ_ONLY` — never mutates user databases.
- **Read-only graph queries**: `graph`, `communities`, `path`, and `explain` all open `index.db` through the shared read-only path, so they do not contend on the writer-side `index.lock`.
- **Transactional index updates**: `apply_changes` and `rebuild` wrap SQLite mutations in nested savepoints, so a failed reindex rolls back instead of leaving partial symbols/edges behind.
- **Explicit WAL checkpoint budget**: writable `index.db` opens set and verify `PRAGMA wal_autocheckpoint=256` so normal write traffic checkpoints the WAL before it grows without a tsift-owned bound.
- **Best-effort indexing is still visible**: unreadable files and symbol/call extraction failures stay non-fatal, but `IndexSummary.warnings` records them and shared index-update paths log them on stderr instead of silently dropping them.
- **Lint stays on the lock-aware read path**: `tsift lint` opens discovered `index.db` files through the shared read-only path with rollback-journal snapshot fallback, so markdown linting keeps working while a live writer is updating the index.
- **Lint auto-discovers the live symbol index**: `tsift lint` now reads the nearest ancestor `.tsift/index.db` plus scoped `.tsift/indexes/*/index.db` files by default, so markdown linting stays wired to the real AST index layout instead of the retired `symbols.db` paths.
- **Prune mode currently favors correctness over skipping**: `tsift index --prune` keeps the compatibility flag and prune stats surface, but it now uses the same full file-mtime walk as normal incremental indexing until tsift has a subtree invalidation strategy that cannot miss in-place file edits.
- **Bounded symbol ranking**: `symbol_search()` only asks SQLite for exact-name rows and overlapping-tag candidates, ordered by match strength and capped to the requested limit, instead of loading the full `symbols` table into memory.
- **Graph CLI stays integration-tested**: `tests/exit_code.rs` drives the compiled binary against an indexed temp project for `search`, `graph`, `communities`, `path`, and `explain`, so graph/query regressions fail above the unit-test layer.
- **Atomic batch edits**: `cmd_edit` validates ALL edits before writing ANY (two-phase).
- **Rewrite protocol**: exit 0 + stdout = rewrite, exit 1 = pass through (matches rtk convention).
- **Search strategy default**: `lexical` for instant results. `hybrid`/`vector` require model download on first run.
- **All public logic tested**: `classify_task`, `apply_edit_op`, `rewrite_command`, SQL helpers all have unit tests.

## Hook Integration

- **Auto-reindex** (`UserPromptSubmit`): `examples/hooks/tsift-autoindex.sh` resolves the git root, runs `tsift index --check --exit-code <root>`, and automatically switches to `--workspace` when the root has `.gitmodules`, so one hook covers initialized submodules. Install via `.claude/settings.json`.
- **Unhooked fallback**: `tsift search` now autoindexes missing or stale indexes by default. Use `--no-autoindex` when you want the old fail-fast stale check instead of a write.
- **Locked freshness prechecks**: search stale checks now use the same rollback-journal snapshot fallback as `tsift status` / `tsift index --check`, so `--scope`, `--federated`, and `--no-autoindex` do not regress back to raw `database is locked` failures.
- **Scoped search fails closed**: `tsift search --scope <name>` now errors before lexical fallback when the submodule name is unknown, instead of silently searching the workspace root with the wrong scope.
- **Inline lock diagnostics**: if `tsift search` autoindex or `tsift index` still loses a write race, stderr now includes the live `lock` / `journal` state, the exact reindex command, and the recommended next step without requiring a separate `tsift locks`.
- **Search rewrite** (`PreToolUse`): `~/.claude/hooks/tsift-rewrite.sh` rewrites `rg`/`grep -r` to `tsift search --strategy lexical`.
- **RTK output filtering** (`PreToolUse`): same hook routes verbose commands (`communities`, `explain`, `graph`, `index`, `search`) through RTK when installed. TOML filters at `~/.config/rtk/filters.toml` cap output lines.
- **Stale-session recovery**: if a resumed tmux or Codex session hits `tsift search timed out ... The index may be stale`, run `tsift index .` and retry the original tsift command.

## Repo

Private: `github.com/btakita/tsift`. Submodule at `src/tsift` in agent-loop.

<!-- tsift:code-navigation -->
## Code Navigation

Run `tsift status` at session start. Use the commands listed in its `use:` output:
- `tsift search <query>` — AST-aware hybrid search (prefer over grep/rg)
- `tsift explain <symbol>` — callers, callees, community context
- `tsift graph <symbol> --callers` / `--callees` — call graph navigation
- `tsift summarize <symbol>` — cached summary (only when listed in `use:`)

If `tsift status` reports a stale index, either run `tsift index .` first or use `tsift search --autoindex ...`. If `tsift search` still times out after that, tsift now kills the timed-out helper search worker before returning; narrow the path/query or retry with a larger `--timeout`.

Only read full source files when tsift results are insufficient.
<!-- /tsift:code-navigation -->
