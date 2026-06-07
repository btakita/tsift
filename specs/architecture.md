# tsift Spec — Architecture & Storage

Part of the [tsift spec](../SPEC.md). See that index for the full command/spec map.

## Architecture

```
tsift (root crate — public package shim: lib.rs + graph/lang/resolution/substrate/libsql_backend
│        modules only `pub use` sibling crates; the package binary delegates to tsift-cli)
├── tsift-core crate (packages/tsift-core — provider-neutral graph types)
│   ├── types module — GraphNode, GraphEdge, GraphProjection, GraphPath, GraphSubgraph
│   │   ├── GraphProvenance, GraphFreshness, GraphPropertyFilter
│   │   ├── GraphQueryOptions, GraphQueryPage, GraphPagedSubgraph
│   │   ├── NeighborhoodScoring, RankedNeighborhoodOptions, RankedNeighborhoodResult
│   │   ├── TerseGraphNode, TerseGraphEdge, TerseGraphSubgraph, TerseSearchHit, TerseHealthScore
│   │   ├── SQLITE_GRAPH_SCHEMA_VERSION (shared schema version constant)
│   │   └── stable_graph_edge_id, graph_edge_id helpers
│   ├── store module — GraphStore trait (CRUD/query contract — lookup, kind scans, neighborhoods, ranked neighborhoods, shortest paths)
│   │   ├── default implementations for edge, paged_edges, neighborhood, ranked_neighborhood, reachable_nodes, resolve_evidence_target
│   │   ├── apply_graph_query_page, apply_graph_edge_query_page paging helpers
│   │   └── shortest_path_using_outgoing path helper
│   ├── convex module — ConvexGraphClient trait, ConvexRowsGraphClient, ConvexGraphStore
│   │   ├── ConvexProjectionRows, ConvexNodeRow, ConvexEdgeRow
│   │   └── GraphProjection::upsert_into, to_convex_rows methods (on lib.rs)
│   └── lib.rs re-exports all public types at crate root for backward compatibility
├── tsift-sqlite crate (packages/tsift-sqlite — SQLite graph store backend)
│   ├── re-exports all tsift-core types for backward compatibility
│   ├── SqliteGraphStore (graph_nodes, graph_edges, graph_node_properties, projection versions, tombstones)
│   ├── SqliteProjectionRefresh, SqliteProjectionVersion, SqliteProjectionRefreshPhase
│   ├── open_graph_read_only_connection, open_graph_read_only_connection_resilient
│   ├── ReadOnlyRecovery, snapshot copy utilities (shared with index module)
│   └── projection boundary for FalkorDB/other read models
├── tsift-libsql crate (packages/tsift-libsql — libSQL graph store backend, optional)
│   ├── LibsqlGraphStore (local and remote libsql/Turso)
│   └── implements GraphStore trait from tsift-core
├── tsift-surrealdb crate (packages/tsift-surrealdb — SurrealDB graph store backend spike, optional/excluded from default workspace)
│   ├── SurrealdbGraphStore (embedded SurrealKV file-backed or in-memory)
│   ├── writes provider-neutral ConvexProjectionRows into SurrealDB records
│   └── implements GraphStore trait from tsift-core behind `backend-surrealdb`
├── tsift-graph crate (packages/tsift-graph — language-aware graph extraction)
│   ├── lang module — Lang enum, Symbol, tree-sitter symbol/call queries, extract_symbols
│   ├── graph extraction — call sites, routes, edge resolution, community detection, shortest path
│   ├── TerseCommunityMember, TerseCommunity, TerseCommunityResult — compact community serialization (name + tagpath_handle only, top-N member slice)
│   ├── complexity module — ComplexityMetrics, LanguageExtractor trait, LanguageRegistry
│   └── re-exported via src/graph.rs and src/lang/mod.rs as thin shims
├── tsift-algorithms crate (packages/tsift-algorithms — graph algorithms)
│   ├── graph_builder module — shared Graph struct, build_graph() (node index + directed adjacency), build_node_index() (node index only); reused by health, dead_code, scc
│   ├── scc module — iterative Tarjan SCC (strongly connected components)
│   ├── health module — composite health score (connectivity, reachability, centrality, cycle risk); terse_health_report computes top/bottom N scores directly without allocating full per-node sub-metrics
│   ├── dead_code module — dead code detection (unreachable, isolated, orphaned nodes)
│   ├── coupling module — coupling analysis (fan-in, fan-out, instability metrics per module)
│   ├── surfaced by `tsift analyze` over the indexed call graph
│   └── re-exported via root `tsift` as `tsift::algorithms`
├── tsift-tokensave crate (packages/tsift-tokensave — tokensave DB reader adapter)
│   ├── TokensaveDb — read-only adapter for .tokensave/tokensave.db
│   ├── schema mapping: tokensave nodes/edges → tsift GraphNode/GraphEdge
│   ├── FTS5 search via nodes_fts virtual table
│   ├── implements GraphStore trait (read-only, write ops bail)
│   ├── selectable through `tsift graph-db --backend tokensave`
│   └── re-exported via root `tsift` as `tsift::tokensave`
├── tsift-resolution crate (packages/tsift-resolution — multi-strategy resolution, scoring, blocklist)
│   ├── scoring module — RankedNeighbor, neighborhood ranking, edge kind scoring, heuristic helpers
│   ├── blocklist module — generated artifact detection, planner config path filtering
│   └── resolve module — F1 scoring, token-overlap ranking, NodeMatchKind, kind priority
├── tsift-quality crate (packages/tsift-quality — quality-gate surfaces)
│   ├── audit module — skill drift detection, manifest reconciliation, usage scanning, cleanup, report writing
│   ├── perf_gate module — perf-gate workload definitions, hop-cap tiers, baseline backend selection
│   ├── dci_benchmark module — DCI (Driven Causal Index) benchmark harness types and result rollups
│   ├── runtime_churn module — RestartChurnState / RestartChurnSummary derivation from transcript events
│   ├── lint module — markdown lint, project root resolution (depends on tsift-index for config + IndexDb)
│   └── re-exported via root `tsift` `pub use tsift_quality::{audit, dci_benchmark, lint, perf_gate, runtime_churn};`
├── tsift-index crate (packages/tsift-index — config + project walk + init + AST symbol index)
│   ├── config module — Config + workspace/submodule resolution
│   ├── walk module — file walking + mtime-based prune semantics + language tagging
│   ├── init module — instruction injection, OpenCode/Codex hook setup, npm package parity
│   ├── index module — AST symbol/index DB management, writer/reader, lock probes, snapshot fallback
│   └── re-exported via root `tsift` `pub use tsift_index::{config, index, init, walk};`
├── tsift-agent-doc crate (packages/tsift-agent-doc — agent-doc/session observability domain)
│   ├── session_cost module — token/runtime-cost digest for Claude JSONL, Codex JSONL, agent-doc logs
│   ├── session_digest module — bounded session transcript/log digest with restart-churn families
│   ├── session_review module — cross-harness aggregate review combining digest + cost + log discovery
│   └── re-exported via root `tsift` `pub use tsift_agent_doc::{session_cost, session_digest, session_review};`
├── tsift-memory crate (packages/tsift-memory — first-party memory substrate)
│   ├── owns `.tsift/memory.db` schema versioning for memory events, summaries, artifacts, tool spans, embeddings, graph links, and import runs
│   ├── exposes token-budgeted handoff planning so observer/plugin prompts split before model calls instead of overflowing
│   ├── exposes a budget guard that rejects oversized raw tool/log/transcript payloads, replaces them with digest/context/session-review commands, and emits retryable chunk plans
│   ├── defines agent-doc hook event contracts for prompt targets, tool artifacts, response summaries, closeout proof, and session-check results
│   ├── projects first-party `.tsift/memory.db` rows into provider-neutral graph nodes/edges (`memory_session`, `memory_event`) plus semantic/source rows for graph-db retrieval
│   ├── reads the observed `claude-mem` SQLite tables (`observations`, `session_summaries`, `user_prompts`) without mutating them and can optionally migrate all supported rows as imported memory events before graph projection, with per-table source/read/import reconciliation
│   ├── exposes a query packet contract for future ranked, token-capped memory retrieval
│   └── re-exported via root `tsift` as `tsift::memory`
├── tsift-session crate (packages/tsift-session — compatibility shim)
│   └── re-exports `tsift-agent-doc::{session_cost, session_digest, session_review}` for existing consumers
├── tsift-summarize crate (packages/tsift-summarize — cached LLM analysis foundation)
│   ├── summarize module — SummaryDb (read-only / read-write opens), entities/relationships/concepts JSON, snapshot fallback for rollback-journal contention, Anthropic API extract pipeline
│   ├── shared by tsift-digest (diff/log/test consume cached summaries) and the tsift-search crate
│   └── re-exported via root `tsift` `pub use tsift_summarize::summarize;`
├── tsift-digest crate (packages/tsift-digest — code-aware digest emitters)
│   ├── diff_digest module — worktree/staged/revision diff digest, touched symbols, call-edge deltas (uses tsift-graph)
│   ├── log_digest module — bounded verbose-log digest, repeat collapse, signal grouping
│   ├── metric_digest module — repeated metric-run deltas + news tables (self-contained)
│   ├── test_digest module — grouped test-failure digest (cargo/pytest)
│   ├── depends on tsift-graph (diff edges + Lang), tsift-quality (lint/runtime_churn), tsift-summarize (SummaryDb enrichment)
│   └── re-exported via root `tsift` `pub use tsift_digest::{diff_digest, log_digest, metric_digest, test_digest};`
├── tsift-search crate (packages/tsift-search — search ranking, impact analysis, tagpath annotation)
│   ├── sift module — local lexical search adapter (ranked BM25-ish lexical hits, cache serialization)
│   │   ├── TokenIndex — inverted token→files index for pre-filtering; skips files with no matching query tokens; cached as token-index.json in cache_dir
│   │   └── Sift::search builds/loads TokenIndex automatically; only reads+scores files in the token-match set
│   ├── impact module — change-impact analysis (call-edge/route/import impacts; per-language import detection gated by lang-* features)
│   ├── tagpath_adapter module — tagpath `.naming/index.json` family/member lookup + handle round-trip
│   ├── depends on tsift-index (config/index/walk), tsift-digest (diff_digest), tsift-graph (Lang), tsift-quality (lint), tsift-summarize
│   ├── forwards lang-* features to tsift-graph (mirrors root tsift) so impact's per-Lang arms compile
│   └── re-exported via root `tsift` `pub use tsift_search::{impact, sift, tagpath_adapter};`
├── tsift-status crate (packages/tsift-status — session health + lock diagnostics)
│   ├── status module — index freshness, instruction-version check, summary-cache recovery, lock-sidecar/journal state
│   ├── backs `tsift status` and `tsift locks`
│   ├── depends on tsift-index (config/index/init), tsift-sqlite (sidecar/recovery helpers), tsift-summarize (SummaryDb)
│   └── re-exported via root `tsift` `pub use tsift_status::status;`
├── tsift-cli crate (packages/tsift-cli — CLI dispatch, command handlers, output formatting)
│   ├── depends directly on sibling `tsift-*` crates; must not depend on the root `tsift` re-export shim
│   ├── clap CLI types — Cli, Commands, GraphDbQuery, output format enums
│   ├── command handlers — cmd_search, cmd_index, cmd_graph, cmd_communities, cmd_analyze, cmd_explain, etc.
│   ├── output formatting — ToolEnvelope, ResponseBudget, terse/schema transforms
│   ├── tagpath annotation — annotate_hits/stored_symbols/edges/communities/path_nodes_with_tagpath
│   ├── traversal graph — TraversalGraphBuild, exploration budget, worker packets
│   ├── convex sync — chunk planning, transport, snapshot diffing
│   └── binary entry point — src/main.rs delegates to tsift_cli::run()

└── rusqlite (storage — existing)
```

## Per-Submodule Isolation

Each git submodule gets its own index. Isolation tiers control federation (cross-submodule queries):

| Tier | Behavior | Examples |
|------|----------|----------|
| **Isolated** | Never federated, strict boundary | private-client, production-secrets |
| **Private** | Never federated | mail, resume |
| **Shared** | Federated by default | agent-doc, corky, ctx-core-dev |

Config: `.tsift/config.toml` in workspace root.

```toml
[defaults]
federation = true

[overrides.session-share]
federation = false
isolated = true

[overrides.mail]
federation = false
```

Workspace scope ids default to the submodule leaf name when it is unique. If two submodules share the same trailing directory name, tsift promotes those scopes to their full `.gitmodules` paths (for example `pkg/app/foo`, `vendor/foo`) so `--scope` / `--submodule` selectors and `.tsift/indexes/<scope>/index.db` stay collision-free. To target one duplicate scope in `.tsift/config.toml`, use the quoted full path key such as `[overrides."vendor/foo"]`.

## Multiplicity Model

tsift treats repository multiplicity as an ordered ownership stack rather than a flat set of paths. The precedence is:

1. repository root — the fallback project boundary and shared runtime artifact root
2. git submodule scope — the privacy/federation boundary used by `.tsift/config.toml`
3. Cargo workspace — the Rust build graph boundary declared by `[workspace]`
4. Cargo package/crate — the source, feature, target, dependency, and test ownership boundary declared by `[package]`
5. language package-manager workspace — future npm/pnpm/yarn/Python workspace boundaries
6. generated/runtime scope — `.tsift`, `.agent-doc`, build output, caches, and other generated paths excluded from source watermarks
7. agent-doc session scope — the document, backlog, queue, worker-result, and source-window boundary used for orchestration

Higher-numbered layers refine lower-numbered ownership without overriding isolation. For example, a Cargo package inside an isolated git submodule can be selected and indexed locally, but it is still excluded from federated search unless the enclosing submodule permits federation. Selectors are deterministic and fail closed: `--scope <selector>` first preserves existing git-submodule matching, then accepts Cargo package selectors by package name, normalized crate name (`foo-bar` and `foo_bar`), relative package root, or manifest path. Duplicate package names promote selectors to relative package roots, mirroring duplicate submodule leaf-name handling.

Cargo multiplicity is projected into the provider-neutral graph as `cargo_workspace` and `cargo_package` nodes. Workspace nodes use `contains_package` edges. Package nodes carry package name, normalized crate name, package root, workspace root, features, targets, and dependency metadata; they link to owned source files with `owns_file`, to manifest dependencies with `declares_dependency`, and to direct Rust `use` / `extern crate` references with `uses_crate`. These edges stay separate from ordinary call edges so `conflict-matrix`, `dependency-dag`, `dispatch-trace`, and graph-db evidence can reason about package ownership and cross-crate coupling without pretending package dependencies are call sites.

## Storage Layout

```
.tsift/
  indexes/
    agent-doc/
      index.db        # SQLite: function signatures, types, locations
      embeddings.lance # LanceDB: vector embeddings of signatures
      deps.json        # call graph + import graph
      meta.json        # last indexed commit, language stats
    corky/
      ...
  config.toml
```

## Multi-Language Architecture

### Distribution: Cargo Feature Flags

Each grammar is a compile-time feature. Default includes all priority languages. Adding a language = grammar crate + feature + query file + `Language` enum variant.

```toml
[features]
default = ["lang-rust", "lang-python", "lang-typescript", "lang-javascript", "lang-kotlin", "lang-zig", "lang-bash", "lang-markdown"]
lang-rust = ["dep:tree-sitter-rust"]
lang-python = ["dep:tree-sitter-python"]
lang-typescript = ["dep:tree-sitter-typescript"]
lang-javascript = ["dep:tree-sitter-javascript"]
lang-kotlin = ["dep:tree-sitter-kotlin"]
lang-zig = ["dep:tree-sitter-zig"]
lang-bash = ["dep:tree-sitter-bash"]
lang-markdown = ["dep:tree-sitter-md"]
all-languages = ["lang-rust", "lang-python", "lang-typescript", "lang-javascript", "lang-kotlin", "lang-zig", "lang-bash", "lang-markdown"]
```

### Grammar Crates

| Language | Crate | Version | Entry Point | Extensions |
|----------|-------|---------|-------------|------------|
| Rust | `tree-sitter-rust` | crates.io | `LANGUAGE` | `.rs` |
| Python | `tree-sitter-python` | crates.io | `LANGUAGE` | `.py`, `.pyi` |
| TypeScript | `tree-sitter-typescript` | crates.io | `LANGUAGE_TYPESCRIPT` | `.ts` |
| TSX | `tree-sitter-typescript` | crates.io | `LANGUAGE_TSX` | `.tsx` |
| JavaScript | `tree-sitter-javascript` | crates.io | `LANGUAGE` | `.js`, `.mjs`, `.cjs` |
| JSX | `tree-sitter-javascript` | crates.io | `LANGUAGE` | `.jsx` |
| Kotlin | `tree-sitter-kotlin-ng` | 1.1.0 | `LANGUAGE` | `.kt`, `.kts` |
| Zig | `tree-sitter-zig` | 1.1.2 | `LANGUAGE` | `.zig` |
| Bash | `tree-sitter-bash` | 0.25.1 | `LANGUAGE` | `.sh`, `.bash`, `.zsh` |
| Markdown | `tree-sitter-md` | 0.5.3 | `LANGUAGE` + `LANGUAGE_INLINE` | `.md`, `.mdx` |

Markdown parsing and heading/list/code-block extraction are owned by the dependency-light `tsift-md-ast` leaf crate. The crate exposes `parse()`, `reparse_incremental()` with serializable `MdTextEdit` source-range edits, `reparse_incremental_with_input_edit()` for callers that already have a tree-sitter edit, and `markdown_symbols_from_tree()` so `tsift-graph` and external live-document consumers share tree-sitter-md behavior without depending on the graph/index stack.

### Language Module Structure

```
src/
  main.rs
  lang/
    mod.rs          # Language enum, extension dispatch, trait definition
    rust.rs         # Rust query patterns + symbol extraction
    python.rs       # Python query patterns
    typescript.rs   # TypeScript + TSX query patterns
    javascript.rs   # JavaScript + JSX query patterns
    kotlin.rs       # Kotlin query patterns
    zig.rs          # Zig query patterns
    bash.rs         # Bash/Zsh/Shell query patterns
    markdown.rs     # Markdown heading/code block extraction
  queries/          # .scm tree-sitter query files (optional — can inline)
```

### Language Enum

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Language {
    #[cfg(feature = "lang-rust")]     Rust,
    #[cfg(feature = "lang-python")]   Python,
    #[cfg(feature = "lang-typescript")] TypeScript,
    #[cfg(feature = "lang-typescript")] Tsx,
    #[cfg(feature = "lang-javascript")] JavaScript,
    #[cfg(feature = "lang-javascript")] Jsx,
    #[cfg(feature = "lang-kotlin")]   Kotlin,
    #[cfg(feature = "lang-zig")]      Zig,
    #[cfg(feature = "lang-bash")]     Bash,
    #[cfg(feature = "lang-markdown")] Markdown,
}

impl Language {
    pub fn from_extension(ext: &str) -> Option<Self> { /* dispatch table */ }
    pub fn tree_sitter_language(&self) -> tree_sitter::Language { /* grammar entry point */ }
    pub fn symbol_query(&self) -> &'static str { /* .scm query for symbol extraction */ }
}
```

### Per-Language Symbol Extraction

| Language | Symbol Types |
|----------|-------------|
| Rust | `fn`, `struct`, `enum`, `trait`, `impl`, `mod`, `type`, `const`, `static` |
| Python | `def`, `async def`, `class`, decorators, module-level assignments |
| TypeScript | `function`, `class`, `interface`, `type`, `enum`, arrow exports |
| TSX | TypeScript symbols + React component detection (JSX elements) |
| JavaScript | `function`, `class`, arrow exports, `module.exports` |
| Kotlin | `fun`, `class`, `interface`, `object`, `data class`, `sealed class`, `enum class`, `companion object` |
| Zig | `fn`, `struct`, `enum`, `union`, `const` |
| Bash | `function`, alias definitions |
| Markdown | headings (h1-h6 with full section spans), list items, fenced code blocks (with language tag and code-body span) |

### SQLite Schema Update

```sql
CREATE TABLE symbols (
    id INTEGER PRIMARY KEY,
    name TEXT NOT NULL,
    kind TEXT NOT NULL,        -- 'function', 'class', 'trait', etc.
    language TEXT NOT NULL,    -- 'rust', 'python', 'typescript', etc.
    signature TEXT,            -- full signature for hover/display
    file TEXT NOT NULL,
    line INTEGER NOT NULL,
    end_line INTEGER,
    parent_module TEXT,
    visibility TEXT            -- 'public', 'private', etc. (language-dependent)
);
CREATE INDEX idx_symbols_name ON symbols(name);
CREATE INDEX idx_symbols_language ON symbols(language);
CREATE INDEX idx_symbols_file ON symbols(file);

CREATE TABLE route_nodes (
    id INTEGER PRIMARY KEY,
    framework TEXT NOT NULL,    -- 'axum', 'actix', 'express', 'nestjs', 'fastapi', 'flask'
    method TEXT,                -- 'get', 'post', ... or NULL for framework defaults
    route_path TEXT NOT NULL,
    handler_name TEXT NOT NULL,
    file TEXT NOT NULL,
    line INTEGER NOT NULL,
    handler_line INTEGER
);
CREATE INDEX idx_route_nodes_path ON route_nodes(route_path);
CREATE INDEX idx_route_nodes_handler ON route_nodes(handler_name);
CREATE INDEX idx_route_nodes_file ON route_nodes(file);

CREATE TABLE dir_state (
    path TEXT PRIMARY KEY,
    mtime_secs INTEGER NOT NULL,
    mtime_nanos INTEGER NOT NULL
);
```

### Transactional Index Updates

`apply_changes` and `rebuild` wrap all SQLite mutations in a SAVEPOINT. If any insert, delete, metadata read, or directory-state write fails mid-batch, the entire mutation is rolled back. The index stays at its pre-call state instead of landing in a partially-updated mix of old and new symbols.

`rebuild` nests its own SAVEPOINT around the inner `apply_changes` SAVEPOINT. If a rebuild fails after the bulk DELETEs but before the re-index finishes, both layers are rolled back and the prior index contents are preserved.

### Large Repo Optimization: Prune Surface Held in Safe Mode

`tsift index --prune` still exists as the compatibility surface for future large-repo optimizations, but it no longer skips subtrees based on directory mtimes.

Directory mtimes are not a sound invalidation signal for in-place file edits: modifying `src/foo.rs` usually changes the file's mtime without changing the parent directory's mtime. The previous subtree-pruning shortcut could therefore miss real source edits and leave the symbol index stale.

**Current behavior:**
1. `dir_state` still records directory mtimes so the persistence surface remains stable
2. `--prune` runs the same full file-mtime walk as normal incremental indexing
3. `prune_stats` remain populated, but active subtree skipping stays at zero until a sound invalidation model exists

**Contract:** correctness wins over speculative skipping. Re-enable subtree pruning only when tsift can prove a directory fingerprint that detects in-place file edits, not just creates/deletes/renames.

**Output includes pruning stats:**
```
Index (prune-safe): 50000 files tracked
  new: 2  modified: 1  deleted: 0  unchanged: 49997 | pruned: 0 dirs (312 walked, 0 files skipped)
```

### Future Evolution: Dynamic Grammar Loading

When grammar count makes binary size a concern (30+ languages), add runtime plugin loading:

```bash
tsift lang install haskell     # download prebuilt .so/.dylib
tsift lang list                # show installed grammars
tsift lang remove haskell      # cleanup
```

Dynamic grammars use `tree_sitter::Language::from_path()`. The `Language` enum and query files work identically — only the loading mechanism changes. Compiled-in grammars (features) take precedence over dynamic ones.

## Development Phases

1. Add `tree-sitter` core + priority grammar crates behind feature flags
2. Implement `Language` enum with extension dispatch
3. Write `.scm` query patterns for each language (Rust + Python first)
4. Implement `tsift index --ast` — multi-language symbol extraction to SQLite
5. Wire AST index into `tsift search` — symbol-match ranking first, BM25 fallback
6. Add remaining language queries (TypeScript, JavaScript, Kotlin, Zig, Bash, Markdown)
7. `tsift-graph` crate — language-aware graph extraction (`Lang`, `Symbol`, call sites, routes, community detection, path finding, `LanguageExtractor` trait, `LanguageRegistry`, `ComplexityMetrics`)
8. Per-submodule config + isolation tiers

## Key Design Decision: Graph > Vector for Code

Aider's repo-map research showed graph-ranked retrieval (PageRank over call/import references) outperforms pure vector similarity for code. The approach: extract symbols via tree-sitter, rank by reference count (centrality), embed only top-ranked. This gives best token efficiency.
