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
tsift index --check <path>      # report stale files without updating the index
tsift index --check --exit-code # exit 1 if stale files found (for scripting/hooks)
tsift index --prune <path>      # skip unchanged directory subtrees (large repo optimization)
tsift graph <path>              # build dependency graph → deps.json
tsift graph --callers <symbol>  # who calls this function?
tsift graph --callees <symbol>  # what does this function call?
tsift communities [--path]      # Louvain community detection over call graph
tsift path <from> <to>          # BFS shortest path between symbols
tsift explain <symbol>          # full symbol context: callers, callees, community
tsift audit                     # scan installed skills, check health
tsift audit --manifest <file>   # compare against expected skill list
tsift summarize <symbol>        # cached LLM summary for a symbol
tsift summarize --extract <path>  # batch LLM extraction (one-time)
tsift summarize --extract --diff  # re-extract only git-changed files
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

## Skill Audit

`tsift audit` scans Claude Code skill directories for health and drift detection.

```bash
tsift audit                              # scan ~/.claude/skills/
tsift audit --skills-dir /path/to/skills # custom directory
tsift audit --manifest skills.txt        # compare against expected list
tsift audit --json                       # structured output
```

**Scan checks per skill:**
- Directory exists and is readable
- `SKILL.md` present, non-empty, has `description` in frontmatter
- Symlink target resolves (detects broken symlinks)

**Manifest comparison** (`--manifest`): cross-references installed skills against an expected list (one name per line, `#` comments allowed). Reports:
- `missing` — listed in manifest but not installed
- `orphan` — installed but not in manifest

**Duplicate detection:** after scanning, `tsift audit` computes pairwise Jaccard similarity over description word sets (stop words filtered) and reports skill pairs with score ≥ 30%. Output:
- Human-readable: `60%  skill-a / skill-b` followed by both descriptions
- JSON: `similar_pairs` array with `skill_a`, `skill_b`, `score` (0.0–1.0), `desc_a`, `desc_b`
- Pairs sorted descending by score
- Skills without descriptions are skipped

**Usage tracking** (`--usage`): scans Claude Code session history (`.jsonl` files in `~/.claude/projects/*/`) for `Skill` tool invocations. Counts per skill, flags never-used skills. Plugin-namespaced skills (`codex:rescue`) are counted under the base name (`codex`). Output:
- Per-skill `invocation_count` field
- JSON: `usage` array sorted descending by count
- Skills invoked but not installed are included in the usage list

**Cleanup recommendations** (`--cleanup`): combines health, usage, and duplicate data into an actionable prune list. A skill is flagged when any of:
- Health issues (broken symlink, missing/empty SKILL.md, no description)
- Zero invocations across all sessions
- ≥50% Jaccard similarity with another skill

Each recommendation includes estimated token savings (total file bytes / 4). Sorted by token savings descending.

**Report** (`--report <path>`): writes a markdown audit report to the given path. Includes skills table (status, name, description, uses), duplicate pairs, manifest diffs, and cleanup recommendations with total savings estimate. Suitable as a nightly cron target.

```bash
tsift audit --usage                          # show invocation counts
tsift audit --cleanup                        # actionable prune list
tsift audit --report audit.md                # write markdown report
tsift audit --usage --cleanup --report r.md  # all features
```

## Markdown Lint

`tsift lint` detects unannotated concepts in markdown files by cross-referencing plain text against known graph entities (symbols from the AST index, headings, bold terms, backtick terms).

```bash
tsift lint README.md                              # lint with auto-discovered entities
tsift lint README.md --entities-from SPEC.md      # add entities from another doc
tsift lint README.md --index .tsift/indexes/tsift # use specific symbol index
tsift lint README.md --json                       # structured output
```

**Entity sources:**
- The file being linted (headings, bold, backtick terms ≥4 chars)
- `--entities-from <path>` markdown files (same extraction)
- `--index <dir>` symbol database (`symbols.db`, names ≥4 chars)
- Default: all symbol databases under `.tsift/indexes/*/symbols.db`

**Detection rules:**
- Skip code blocks, headings, and HTML comments
- Skip already-annotated terms (backtick-wrapped, bold-wrapped, link text, inside inline code)
- Require word boundaries (no partial matches)
- Classify suggestions: `symbol` → backtick, multi-word capitalized → link, other → bold

**Output:**
- Human-readable: `file:line:col: text → suggestion`
- JSON: `annotations` array with `line`, `column`, `text`, `entity`, `kind`, `suggestion`

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

## Summarize (Cached LLM Analysis)

`tsift summarize` provides token-efficient access to pre-computed LLM analysis. Pay once for extraction, query free thereafter.

```bash
tsift summarize <symbol>            # show cached summary for a symbol
tsift summarize --file <path>       # show cached summary for a file/module
tsift summarize --extract <path>    # run LLM extraction on path (batch)
tsift summarize --extract --diff    # re-extract only git-changed files
tsift summarize --stats             # cache hit rate, staleness, token savings
tsift summarize --json              # structured output
```

### Architecture

```
tsift summarize
├── extract (one-time, per file content hash)
│   ├── reads source + AST symbols from symbols.db
│   ├── calls Anthropic batch API (haiku for cost)
│   └── stores: entities, relationships, summaries → summaries.db
├── query (instant, local SQLite)
│   ├── by symbol name → summary + relationships + community context
│   ├── by file path → module-level summary + exported entities
│   └── by concept → cross-file entity matches
└── invalidation
    ├── cache key: blake3(file_content) + symbol_name
    ├── --diff mode: only re-extracts files changed since last extraction
    └── stale entries kept readable, marked for re-extraction
```

### Storage Schema (summaries.db)

```sql
CREATE TABLE summaries (
    id INTEGER PRIMARY KEY,
    symbol_name TEXT NOT NULL,
    file_path TEXT NOT NULL,
    content_hash TEXT NOT NULL,      -- blake3 of source file at extraction time
    summary TEXT NOT NULL,           -- 1-3 sentence description
    entities TEXT,                   -- JSON array of extracted entities
    relationships TEXT,              -- JSON array of {from, to, kind}
    concept_labels TEXT,             -- JSON array of domain concepts
    extracted_at TEXT NOT NULL,      -- ISO timestamp
    model TEXT NOT NULL,             -- model used for extraction
    tokens_input INTEGER,           -- tokens consumed during extraction
    tokens_output INTEGER
);
CREATE INDEX idx_summaries_symbol ON summaries(symbol_name);
CREATE INDEX idx_summaries_file ON summaries(file_path);
CREATE INDEX idx_summaries_hash ON summaries(content_hash);
```

### Extraction Protocol

1. Collect target files (from path arg or `--diff` against `git diff --name-only`)
2. For each file, load source + symbols from `symbols.db`
3. Build extraction prompt: source snippet + symbol list + "extract entities, relationships, 2-sentence summary"
4. Submit via Anthropic batch API (haiku-class model, 50% cost vs synchronous)
5. On batch completion, parse responses and insert/update `summaries.db`
6. Report: files processed, entities found, tokens spent, estimated savings

### Token Savings Model

Without summarize: reading a 500-line file costs ~2000 tokens per context load.
With summarize: loading the cached summary costs ~50-100 tokens. Savings compound across repeated queries in a session.

`--stats` reports: total extractions, cache hits vs misses, estimated tokens saved across sessions.

### Boundary Rule

`tsift summarize` owns cached, pre-computed analysis that's deterministic after extraction. It does NOT:
- Run live LLM calls at query time (extraction is batch-only)
- Generate new analysis on cache miss (returns "not extracted" + suggests `--extract`)
- Own visualization or graph rendering (leave to graphify)

### Configuration

```toml
# .tsift/config.toml
[summarize]
model = "claude-haiku-4-5-20251001"  # extraction model
batch = true                          # use batch API (50% savings)
max_file_tokens = 8000               # skip files larger than this
api_key_env = "ANTHROPIC_API_KEY"    # env var for API key
```

## Hook Integration

### Auto-Reindex (`UserPromptSubmit`)

`tsift index --check --exit-code` enables scripted freshness checks. The `--exit-code` flag makes `--check` exit 1 when stale files exist (new, modified, or deleted since last index) and exit 0 when fresh. Without `--exit-code`, `--check` always exits 0.

**Claude Code hook** (`.claude/settings.json`):

```json
{
  "hooks": {
    "UserPromptSubmit": [
      { "matcher": "", "command": "examples/hooks/tsift-autoindex.sh" }
    ]
  }
}
```

The hook runs `tsift index --check --exit-code .` silently on every prompt. If the index is stale, it runs `tsift index .` to rebuild incrementally. When the index is fresh, the check completes in ~50ms with no side effects.

For workspace mode, replace `. ` with `--workspace` in the hook.

### Search Rewrite (`PreToolUse`)

The existing `tsift-rewrite.sh` hook intercepts `rg`/`grep -r` Bash calls and silently rewrites them to `tsift search --strategy lexical`. See `~/.claude/hooks/tsift-rewrite.sh`.

## What NOT to build

- Visualization (Mermaid, HTML) — leave to graphify
- Full LSP-level type inference — diminishing returns
- Embedding model hosting — use external API or lightweight local model (all-MiniLM-L6-v2)
- Dynamic grammar loading (until binary size exceeds ~50MB)
- Live LLM calls at query time in `tsift summarize` — extraction is batch-only
