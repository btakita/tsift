# Spec: AST-Aware Code RAG

## Goal

Extend tsift with tree-sitter AST parsing, dependency graph tracking, and per-submodule isolation to enable token-efficient code retrieval at the function/symbol level rather than file level.

## Architecture

```
tsift (CLI + MCP plugin)
├── sift (BM25 + vector — existing)
├── graph module (internal — src/graph.rs)
│   ├── call-site extraction via tree-sitter queries
│   ├── caller→callee edge resolution against symbol table
│   └── SQLite storage (call_edges table)
├── lang module (tree-sitter parsing — existing)
│   ├── symbol extraction (function/type/trait definitions)
│   └── call queries (function calls, method calls, macro invocations)
└── rusqlite (storage — existing)
```

## Per-Submodule Isolation

Each git submodule gets its own index. Isolation tiers control federation (cross-submodule queries):

| Tier | Behavior | Examples |
|------|----------|----------|
| **Isolated** | Never federated, strict boundary | session-share, monsterrodholders-dev |
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

## Storage Layout

```
.tsift/
  indexes/
    agent-doc/
      symbols.db      # SQLite: function signatures, types, locations
      embeddings.lance # LanceDB: vector embeddings of signatures
      deps.json        # call graph + import graph
      meta.json        # last indexed commit, language stats
    corky/
      ...
  config.toml
```

## New Subcommands

```bash
tsift index --ast <path>        # tree-sitter AST extraction → symbols.db
tsift index --prune <path>      # skip unchanged directory subtrees (large repo optimization)
tsift graph <path>              # build dependency graph → deps.json
tsift graph --callers <symbol>  # who calls this function?
tsift graph --callees <symbol>  # what does this function call?
tsift communities [--path]      # Louvain community detection over call graph
tsift path <from> <to>          # BFS shortest path between symbols
tsift explain <symbol>          # full symbol context: callers, callees, community
tsift search <query>            # gains AST-aware ranking when index exists
tsift search --scope <submod>   # restrict to one submodule's index
```

## Community Detection (Louvain)

`tsift communities` clusters the call graph into architectural subsystems using the Louvain method.

```bash
tsift communities [--path <path>] [--scope <submod>] [--min-size N] [--json]
```

**Algorithm:** greedy modularity optimization over an undirected, deduplicated call graph.
1. Each symbol starts in its own community
2. For each node, compute modularity gain of moving to each neighbor's community
3. Move to the best community if gain > 0
4. Repeat until convergence (no improving moves or 100 iterations)

**Output:** communities sorted by size (largest first), total modularity Q ∈ [-0.5, 1.0] (higher = stronger community structure), per-community member list and modularity contribution.

**`--min-size N` (default 2):** filter out singleton communities (external symbols with no definition in the indexed codebase).

**Boundary rule:** `tsift communities` owns deterministic, AST-derived clustering. For LLM-derived semantic groupings (concept clusters, domain labels), use graphify's semantic layer over `tsift graph --json` output.

## Graph Path Queries

### Shortest Path

`tsift path <from> <to>` finds the shortest path between two symbols using BFS over the undirected call graph. Useful for understanding how distant parts of a codebase are connected.

```bash
tsift path cmd_index apply_changes          # show connection chain
tsift path cmd_index apply_changes --json   # structured output
tsift path cmd_index apply_changes --scope sub  # restrict to submodule
```

The graph is treated as undirected — if A calls B, the path A→B and B→A are both valid hops. Returns null/message when no path exists (disconnected components).

### Symbol Explanation

`tsift explain <symbol>` provides full context for a symbol: definitions, callers, callees, and community membership.

```bash
tsift explain main              # full context for 'main'
tsift explain main --json       # structured output
tsift explain main --scope sub  # restrict to submodule
```

Community membership is computed on-the-fly via Louvain to show which architectural subsystem the symbol belongs to.

## Key Design Decision: Graph > Vector for Code

Aider's repo-map research showed graph-ranked retrieval (PageRank over call/import references) outperforms pure vector similarity for code. The approach: extract symbols via tree-sitter, rank by reference count (centrality), embed only top-ranked. This gives best token efficiency.

## Output Contract

Retrieval returns function-level results, not file-level:
- Function signature + file:line location
- 1-hop dependencies (callers/callees)
- 50-200 tokens per result vs. 2000+ for full-file reads

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
| Markdown | headings (h1-h6), fenced code blocks (with language tag) |

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

CREATE TABLE dir_state (
    path TEXT PRIMARY KEY,
    mtime_secs INTEGER NOT NULL,
    mtime_nanos INTEGER NOT NULL
);
```

### Large Repo Optimization: Directory mtime Pruning

For repos with 100K+ files, `tsift index --prune` skips unchanged directory subtrees during the file walk. Directory mtime changes when files are created, deleted, or renamed within it. When a directory's mtime matches stored state, the entire subtree is skipped.

**How it works:**
1. `dir_state` table stores directory modification times after each index run
2. On subsequent runs with `--prune`, the walker checks each directory's mtime against stored state
3. Directories with unchanged mtime are pruned — their files are treated as unchanged
4. Files in pruned directories are not stat'd or re-parsed

**Tradeoff:** In-place file content modifications do not update directory mtime. The `--prune` flag may miss modified files in unchanged directories. Use periodic `--rebuild` for full accuracy, or omit `--prune` when precision matters.

**Output includes pruning stats:**
```
Index (pruned): 50000 files tracked
  new: 2  modified: 1  deleted: 0  unchanged: 49997 | pruned: 312 dirs (8 walked, 49500 files skipped)
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
7. ~~Add `tsift-graph` crate~~ → Internal `graph` module — call graph extraction + edge storage (done)
8. Per-submodule config + isolation tiers

## What NOT to build

- Visualization (Mermaid, HTML) — leave to graphify
- Full LSP-level type inference — diminishing returns
- Embedding model hosting — use external API or lightweight local model (all-MiniLM-L6-v2)
- Dynamic grammar loading (until binary size exceeds ~50MB)
