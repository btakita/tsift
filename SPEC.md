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

Workspace scope ids default to the submodule leaf name when it is unique. If two submodules share the same trailing directory name, tsift promotes those scopes to their full `.gitmodules` paths (for example `pkg/app/foo`, `vendor/foo`) so `--scope` / `--submodule` selectors and `.tsift/indexes/<scope>/index.db` stay collision-free. To target one duplicate scope in `.tsift/config.toml`, use the quoted full path key such as `[overrides."vendor/foo"]`.

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

## New Subcommands

```bash
tsift index --ast <path>        # tree-sitter AST extraction → index.db
tsift index --check <path>      # report stale files without updating the index
tsift index --check --exit-code # exit 1 if stale files found (for scripting/hooks)
tsift index --check --quiet     # summary only — omit per-file change list
tsift index --prune <path>      # conservative full scan; reserved prune surface until subtree invalidation is sound
tsift graph <path>              # build dependency graph → deps.json
tsift graph --callers <symbol>  # who calls this function?
tsift graph --callees <symbol>  # what does this function call?
tsift communities [--path]      # Louvain community detection over call graph
tsift path <from> <to>          # BFS shortest path between symbols
tsift explain <symbol>          # full symbol context: callers, callees, community
tsift edit < edits.json         # staged multi-file search/replace batch
tsift audit                     # scan installed skills, check health
tsift audit --manifest <file>   # compare against expected skill list
tsift summarize <symbol>        # cached LLM summary for a symbol
tsift summarize --extract <path>  # batch LLM extraction (one-time; relative path resolves against --path, workspace files use the matching scoped index)
tsift summarize --extract --diff  # re-extract only git-changed files within the requested path
tsift search <query>            # lexical by default; gains AST-aware ranking when index exists
tsift search --autoindex <query> # opt-in: build/rebuild the local index before search
tsift search --scope <submod>   # restrict to one submodule's index + lexical root
tsift index --submodule <submod> # unknown/ambiguous workspace scopes fail closed
tsift search --strategy hybrid  # opt-in to slower hybrid BM25 + vector search
tsift search --timeout 60       # custom timeout in seconds (default: 30, 0 = no timeout)
tsift --compact search <query>  # terse human output across commands
```

`tsift summarize --stats`, `tsift summarize <symbol>`, and `tsift summarize --file <path>` are read-only cache queries: they fail closed when `.tsift/summaries.db` is absent, never create the summary cache as a side effect, and retry against a snapshot copy when a rollback-journal lock wedges the live cache. `--path` first resolves through the nearest ancestor `.tsift` project/workspace root, so nested directories reuse the shared summary cache instead of creating shadow caches; `summarize --file` also normalizes equivalent path spellings back to the canonical root-relative cache key, so `src/lib.rs`, `./src/lib.rs`, nested relative spellings that point at the same file, and absolute paths routed through a symlinked checkout all hit the same cached row. Summary cache rows store that root-relative key with `/` separators even on Windows, and read/delete/currentness checks also tolerate legacy `\` rows until they are rewritten. During `--extract`, relative extract paths resolve against the caller's `--path` anchor (or that file's parent directory), then canonicalize when possible and otherwise collapse lexical `.` / `..` segments before diff filtering, stale-row pruning, and cache-key derivation, while still reusing the ancestor project's shared summary cache. tsift claims an exclusive sibling `summaries.lock` sidecar before it deletes stale rows, rechecks content hashes, or calls the LLM so concurrent extractors fail fast instead of duplicating API spend, full re-extracts prune cached summary rows for files that no longer exist inside the requested extract scope even when that scope is now empty, workspace files resolve symbol context against the matching scoped `index.db`, symbol preload uses exact normalized file-path matches so duplicate `src/lib.rs`-style paths across scopes do not bleed into each other, symbol preload reuses the same busy-timeout plus snapshot fallback path as other read-only index consumers when a rollback-journal writer is live, and `--diff` includes untracked files within the requested extract scope while deleting cached summary rows for tracked files that were removed from that scope, including the old side of `git mv` renames; on an unborn `HEAD`, `--diff` degrades to untracked-only extraction instead of failing on `git diff ... HEAD`. `tsift status` computes summary coverage against live indexed files only, so stale summary rows for deleted files do not over-report cache coverage, and it surfaces summary-cache recovery diagnostics when it had to degrade off the live database.

`tsift edit` now stages each rewritten file beside its target and only swaps the batch into place after every edit validates and every staged file is ready. If any later swap fails, tsift restores earlier files before returning an error instead of leaving a partially-written batch behind.

## Search Stale Precheck + Timeout

`tsift search` now performs a cheap freshness precheck before it calls the sift engine. If an existing local index is stale, search fails fast instead of spending up to 30 seconds in the lexical engine first.

Default behavior:

- fresh index: search proceeds normally
- stale index: search exits non-zero immediately and tells the user to run `tsift index ...`
- missing index: search still proceeds, but symbol ranking stays unavailable until the project is indexed

Opt-in recovery:

- `tsift search --autoindex ...` mirrors the hook behavior for unhooked sessions: if the local or scoped index is missing or stale, tsift incrementally builds it before searching
- `tsift search --scope <submod> --autoindex ...` rebuilds only that submodule's index
- `tsift search --federated --autoindex ...` rebuilds stale/missing federated submodule indexes before aggregating symbol hits, and its lexical/vector/hybrid sift pass only searches the same federated scope roots instead of the whole workspace
- `tsift search --scope <submod> ...` now fails closed when the named submodule does not exist, and reports the available scope ids instead of silently searching the workspace root
- `tsift index --submodule <submod> ...` now fails closed on that same unknown or ambiguous selector set, instead of indexing `root/<submod>` into an unreachable scoped DB
- when duplicate submodules share the same trailing directory name, leaf-name selectors fail closed as ambiguous and the full `.gitmodules` path becomes the required scope id
- `tsift status`, `tsift search`, `tsift index`, `tsift locks`, `graph`, `communities`, `path`, and `explain` now resolve nested input paths against the nearest ancestor project/workspace root (`.tsift/` or workspace `.gitmodules`), so subdirectory invocations reuse the intended project/workspace indexes instead of creating nested `.tsift/index.db` state or inspecting synthetic nested lock files
- when a nested workspace path already falls under exactly one submodule source root, `tsift search`, `tsift locks`, `graph`, `communities`, `path`, and `explain` now infer that scoped index automatically instead of requiring a redundant `--scope <scope>` selector
- workspace roots that only have scoped `.tsift/indexes/<scope>/index.db` files now make plain `tsift search` fail closed until the caller picks `--scope <scope>` or `--federated`, instead of auto-creating a second shared root index layout
- workspace roots that only have scoped `.tsift/indexes/<scope>/index.db` files now make `graph`, `communities`, `path`, and `explain` fail closed until the caller picks `--scope <scope>`, instead of surfacing a misleading missing-root-index error
- `tsift search` symbol-hit reads now reopen `index.db` through the same resilient read-only helper used by other index consumers, so a rollback-journal lock that appears after the stale-index precheck still falls back to a snapshot copy instead of bubbling a raw SQLite lock error
- writable index updates now claim an OS-backed exclusive lock on the sibling `index.lock` sidecar first, so concurrent `tsift index` / `tsift search --autoindex` writers fail fast with a tsift-owned error instead of surfacing raw SQLite lock contention or PID-recycling false positives
- read-only graph queries (`graph`, `communities`, `path`, `explain`) open `index.db` without taking that writer-side `index.lock`, and when a rollback-journal writer wedges the live database they retry against a snapshot copy so diagnostic and graph traversal commands stay available
- writable `index.db` opens also set `PRAGMA wal_autocheckpoint=256`, so normal tsift write traffic checkpoints the WAL on an explicit budget instead of leaving it entirely to SQLite defaults
- non-fatal source-read / symbol-extraction / call-extraction failures now emit warnings instead of being silently swallowed, and those warnings are carried in `IndexSummary` for JSON consumers

`tsift search` still wraps the sift engine call in a 30-second timeout (configurable via `--timeout`). Timed searches now run in an internal helper process so a timeout kills the underlying sift work instead of leaving a detached worker thread behind. The timeout remains a backstop for genuinely slow lexical searches or for sessions that reach search without a usable index.

When an index is present, the AST symbol-ranking prepass is now bounded: SQLite only pulls exact-name rows and overlapping-tag candidates, orders them by exact/tag overlap, and caps that candidate scan to the requested search `--limit` instead of loading the full `symbols` table into memory first.

On stale existing indexes, search exits early with a message like:
```
tsift search aborted: index is stale (51 files). Run `tsift index .` or re-run with `--autoindex`.
```

If the sift engine itself still times out, search exits with a non-zero code and prints:
```
tsift search timed out after 30s (strategy: lexical). The index may be stale — run `tsift index .` to rebuild, or use `--timeout 0` to disable the timeout.
```

`--timeout 0` disables the timeout for cases where a long search is expected and keeps the sift call in-process.

## Index Quiet Mode

`tsift index --quiet` (or `-q`) suppresses the per-file change list, printing only the summary line. `--exit-code` implies `--quiet`.

Without `--quiet`, `tsift index --check` on a large repo with 14K+ stale files outputs every file path (1.7MB / 433K tokens in human mode, 2.6MB in JSON). With `--quiet`, output is a single summary line (~80 bytes human, ~120 bytes JSON).

In JSON mode, `--quiet` also omits the `changes` array and uses compact (non-pretty) serialization.

## Global Compact Output

`tsift --compact` is a global flag for human-readable output. It keeps the underlying command behavior the same, but trims verbose formatting across commands:

- `search` drops metadata banners, keeps one-line snippets, reduces score precision, abbreviates kind/match_type labels, uses `syms[N]:` header
- `explain` groups callers/callees by file instead of repeating the same path per edge; abbreviates kind labels; uses `sym:`, `crs[N]:`, `ces[N]:`, `comm[N]:` headers
- `graph` uses `crs[N]:` / `ces[N]:` headers
- `communities` shows top members per cluster with `(+N more)` instead of full dumps; uses `comms n:N e:N iter:N q:Q cnt:N` header with `mbrs` label
- `path`, `status`, `audit`, `summarize`, `lint`, `sql`, and `index` switch to denser summary-oriented layouts

### Compact Abbreviation Conventions

In `--compact` mode, common labels are shortened:

| Full | Abbreviated | Context |
|------|------------|---------|
| `function` | `fn` | symbol kind |
| `method` | `meth` | symbol kind |
| `class` | `cls` | symbol kind |
| `interface` | `iface` | symbol kind |
| `type_alias` | `type` | symbol kind |
| `data_class` | `data_cls` | symbol kind |
| `sealed_class` | `sealed_cls` | symbol kind |
| `enum_class` | `enum_cls` | symbol kind |
| `companion_object` | `comp_obj` | symbol kind |
| `object` | `obj` | symbol kind |
| `heading` | `h` | symbol kind |
| `code_block` | `code` | symbol kind |
| `exact_name` | `exact` | match type |
| `partial_tags` | `partial` | match type |
| `symbols` | `syms` | section header |
| `callers` | `crs` | section header |
| `callees` | `ces` | section header |
| `community` | `comm` | section header |
| `communities` | `comms` | section header |
| `members` | `mbrs` | section header |
| `symbol` | `sym` | section header |
| `definitions` | `defs` | section header |

Short kinds (`struct`, `trait`, `enum`, `const`, `static`, `mod`, `impl`, `alias`, `union`) pass through unchanged.

`--compact` does not change `--json` formatting. Use `--pretty` for indented JSON.

## Compact JSON Default

All `--json` output uses compact (single-line) serialization by default. This saves 30-50% of tokens compared to pretty-printed JSON.

`tsift --pretty` is a global flag that switches JSON output to indented (pretty-printed) format for human readability. Without `--pretty`, JSON is compact.

```bash
tsift search "main" --json                # compact JSON (default)
tsift --pretty search "main" --json       # pretty-printed JSON
tsift --pretty explain main --json        # pretty-printed JSON
```

## Terse JSON Mode

`tsift --terse` is a global flag that outputs JSON with abbreviated field names and an inline schema header. It implies `--json` for any command that supports it.

Output format: `{"_s": {<short→long mapping>}, "d": <data with short keys>}`. The `_s` schema only includes keys that appear in the current response.

```bash
tsift --terse search "main"               # terse JSON (implies --json)
tsift --terse explain main                 # terse JSON
tsift --terse --pretty status .            # terse + pretty-printed
```

**Key mappings** (subset — full list in source):

| Long | Short | Long | Short |
|------|-------|------|-------|
| `caller_file` | `cf` | `caller_name` | `cn` |
| `callee_name` | `en` | `call_site_line` | `csl` |
| `name` | `n` | `kind` | `k` |
| `file` | `f` | `line` | `l` |
| `language` | `la` | `score` | `sc` |
| `end_line` | `el` | `match_type` | `mt` |
| `symbol` | `s` | `symbols` | `sy` |
| `callers` | `crs` | `callees` | `ces` |
| `community` | `cm` | `communities` | `cms` |
| `modularity` | `q` | `members` | `m` |
| `hits` | `h` | `snippet` | `sn` |
| `path` | `p` | `definitions` | `df` |

Unknown keys pass through unchanged.

## Tabular Output

`tsift --tabular` is a global flag that outputs repeated structures as TSV (tab-separated values) with a header row. Designed for structured, token-efficient display that agents and scripts can parse without JSON overhead.

**Supported commands:**
- `search` — symbols table (`match_type`, `kind`, `name`, `file`, `line`, `score`) then hits table (`rank`, `path`, `confidence`, `score`)
- `graph` — edges table (`direction`, `name`, `file`, `line`) with `caller`/`callee` in the direction column
- `communities` — table (`id`, `size`, `members`) where members are comma-separated
- `explain` — definition table (`section`, `kind`, `name`, `file`, `line`) then edges table, then community summary

Truncation is indicated by `# (+N more)` comment lines. Sections are separated by blank lines.

```bash
tsift --tabular search "main"              # two TSV tables: symbols + hits
tsift --tabular graph main --callers       # one TSV table: direction name file line
tsift --tabular communities --limit 5      # one TSV table: id size members
tsift --tabular explain main               # definition + edges + community
```

## Schema-Then-Values Mode

`tsift --schema` is a global flag that converts arrays of same-structured objects into a columnar format: column names once, then rows as value arrays. Implies `--json`.

Output format: for an array of objects with keys `[k1, k2, k3]`, produces `{"_c": [k1, k2, k3], "_r": [[v1, v2, v3], ...]}`.

**Rules:**
- Arrays of 2+ objects with identical key sets are converted
- Arrays with 1 element, heterogeneous keys, or non-object elements pass through unchanged
- Applied recursively to nested objects
- Combines with `--terse`: abbreviated field names in `_c`, plus `_s` schema mapping
- Combines with `--pretty` for indented output

```bash
tsift --schema search "main"               # schema-then-values JSON
tsift --schema --terse search "main"       # abbreviated keys + columnar
tsift --schema --pretty explain main       # indented columnar JSON
```

**Example output (`--schema`):**
```json
{"symbols":{"_c":["kind","line","name"],"_r":[["fn",10,"main"],["fn",20,"helper"]]}}
```

**Example output (`--schema --terse`):**
```json
{"_s":{"k":"kind","l":"line","n":"name"},"d":{"sy":{"_c":["k","l","n"],"_r":[["fn",10,"main"],["fn",20,"helper"]]}}}
```

## Relative Paths (Default)

All file paths in output are project-relative by default. The project root is detected via `path.canonicalize()` in each command. Relative paths are shorter and save tokens — tsift's core mission.

`tsift --absolute` is a global flag that switches output to absolute paths for cases where the full filesystem path is needed (e.g., piping to external tools).

```bash
tsift search "main"                        # paths: src/main.rs
tsift --absolute search "main"             # paths: /home/user/project/src/main.rs
tsift explain main                         # file: src/main.rs, caller_file: src/lib.rs
tsift --absolute graph main --callers      # full paths in output
```

**Scope:** applies to all commands that emit file paths — `search`, `graph`, `explain`, `index`, `summarize`, `lint`. Commands that only emit symbol names (`communities`, `path`) are unaffected.

**JSON output:** path-bearing keys (`file`, `path`, `caller_file`, `file_path`) are stripped in both regular and terse JSON. Non-path string values are never modified.

**Database storage:** paths remain absolute in SQLite. Stripping happens at output time only, so `--absolute` is a display toggle, not a data migration.

## Output Caps (`--limit N`)

Per-command output limits prevent large codebases from flooding agent context windows.

| Command | Flag | Default | What it caps |
|---------|------|---------|-------------|
| `graph` | `--limit N` | 20 | Edges per direction (callers, callees) |
| `communities` | `--limit N` | 10 | Number of communities displayed |
| `explain` | `--limit N` | 15 | Callers and callees each |

`--limit 0` disables the cap (show everything). When output is truncated, a `(+N more)` suffix appears in text mode and `truncated: true` + `*_total` fields appear in JSON.

```bash
tsift graph main --limit 5            # max 5 callers + 5 callees
tsift explain main --limit 0          # show all callers/callees
tsift communities --limit 3           # top 3 communities only
```

**JSON truncation fields:** `total` (or `callers_total`/`callees_total` for graph/explain) gives the full count before truncation. `truncated` (or `callers_truncated`/`callees_truncated`) is a boolean.

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

**Locking:** `tsift communities` is a read-only graph query. It opens the existing `index.db` without acquiring the writer-side `index.lock`, and if a rollback-journal writer temporarily blocks live reads it retries against a snapshot copy so the command remains available.

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

## Graph CLI End-to-End Coverage

The graph-oriented CLI surface should stay covered through the compiled binary, not just unit helpers. `tests/exit_code.rs` owns a real temp-project fixture that runs:

- `tsift search --json`
- `tsift graph --json`
- `tsift communities --json`
- `tsift path --json`
- `tsift explain --json`

Keep that fixture aligned with the command output contracts so changes in indexing, graph extraction, or JSON rendering fail in one integration layer before release.

## Release Workflow

tsift release automation is tag-driven:

- `push` of a `vX.Y.Z` tag runs the release workflow
- the workflow fails closed if the tag does not exactly match `Cargo.toml` `package.version`
- release verification includes `cargo clippy --all-targets -- -D warnings` and `cargo test`
- successful tagged releases attach prebuilt archives plus `.sha256` checksum files to the matching GitHub Release
- prebuilt binaries are emitted for `x86_64-unknown-linux-gnu`, `x86_64-apple-darwin`, `aarch64-apple-darwin`, and `x86_64-pc-windows-msvc`
- the crates.io publish job exists but is gated behind the `TSIFT_ENABLE_CRATES_PUBLISH=true` repo variable so normal GitHub releases do not fail on the current upstream packaging blocker

Current blocker: tsift's search engine dependency comes from `github.com/rupurt/sift`, but that library is not published on crates.io under a compatible package name. The existing crates.io `sift` crate is a different project. Until the upstream dependency is published under a consumable crates.io package name and `Cargo.toml` is retargeted to it, crates.io publishing must remain explicitly disabled.

To keep the remaining dependency surface publish-ready, any dependency that uses a local `path` source should also carry the matching crates.io `version` requirement whenever that published crate already exists.

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
tsift lint README.md --index .tsift               # use a specific project index root
tsift lint README.md --index .tsift/indexes       # use a scoped-index directory directly
tsift lint README.md --json                       # structured output
```

**Entity sources:**
- The file being linted (headings, bold, backtick terms ≥4 chars)
- `--entities-from <path>` markdown files (same extraction)
- `--index <dir>` live symbol index discovery (`index.db`, names ≥4 chars) from a project root, `.tsift` directory, `.tsift/indexes`, scope directory, or direct `index.db` path
- Explicit `indexes` directories recurse through nested scope-id paths (for example `indexes/pkg/app/foo/index.db`), so duplicate-leaf workspace exports stay lintable even outside the original workspace root
- When `--index` points at a workspace aggregate target (workspace root, `.tsift`, `.tsift/index.db`, or `.tsift/indexes`), `tsift lint` applies the same federation filter as auto-discovered roots. Private, isolated, and explicitly non-federated scopes are excluded unless the caller points `--index` at that specific scope directory or `index.db`.
- Workspace aggregate discovery only reads `.tsift`-owned indexes; an unrelated repo-root `index.db` is ignored unless the caller passes that file path explicitly.
- Default: the nearest ancestor project root with `.tsift/index.db`, plus only scoped indexes under `.tsift/indexes/**/index.db` whose workspace scope still participates in federation. Private, isolated, or explicitly non-federated scopes are excluded unless the caller points `--index` at them directly.

**Locking:**
- `tsift lint` opens discovered `index.db` files through the shared read-only path with rollback-journal snapshot fallback, so lint stays available while a live writer has the index locked.

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
7. ~~Add `tsift-graph` crate~~ → Internal `graph` module — call graph extraction + edge storage (done)
8. Per-submodule config + isolation tiers

## Init (Project Setup)

`tsift init` ensures the Code Navigation section is present in `AGENTS.md` for Codex-style harnesses and mirrors it into `CLAUDE.md` when that file exists, so local agent sessions prefer tsift over raw file reads.

```bash
tsift init                              # ensure AGENTS.md (and CLAUDE.md if present) in current directory
tsift init <path>                       # inject at <path> (dir or file)
tsift init src/sub/tasks/plan.md        # resolves to submodule root src/sub/
tsift init --codex                      # also inject auto-reindex hook into .codex/hooks.json
tsift init --codex --workspace          # resolve to workspace root + install one workspace hook
```

### Path Resolution

`tsift init` resolves the target directory before operating:

1. If `<path>` is a file, use its parent directory
2. Run `git rev-parse --show-toplevel` from that directory to find the git root (handles submodules)
3. Fall back to the directory itself if not in a git repo

This means `tsift init src/session-share/tasks/claudescore-3.md` resolves to `src/session-share/` — the submodule root — and initializes there. When the resolved path differs from the input, a `resolved: <input> → <target>` line is printed.

With `--workspace`, `tsift init` first checks `git rev-parse --show-superproject-working-tree`. When invoked inside a submodule, that promotes the target to the parent workspace root before the normal git-root fallback.

### Behavior

1. Adds `.tsift/` to `.gitignore` (creates the file if needed, appends if entry missing, skips if already present)
2. Ensures `AGENTS.md` exists with the section (creates it if needed)
3. If `CLAUDE.md` exists, updates or appends the same section there too
4. If the section already exists (detected by `<!-- tsift:code-navigation -->` markers), updates it in place
5. Idempotent — running twice produces no changes on the second run
6. With `--codex`: merges a `UserPromptSubmit` auto-reindex hook into `.codex/hooks.json` (creates the file and directory if needed, updates stale tsift commands in place, removes duplicate tsift hook entries, idempotent)
7. When the resolved target has `.gitmodules`, the Codex hook automatically uses `tsift index --check --exit-code --workspace <root>` / `tsift index --workspace <root>` so one root hook covers initialized submodules. `--workspace` makes that root resolution explicit from inside a submodule.

### Injected Section

```markdown
<!-- tsift:code-navigation v=0.1.0 -->
## Code Navigation

Run `tsift status` at session start. Use the commands listed in its `use:` output:
- `tsift search <query>` — AST-aware hybrid search (prefer over grep/rg)
- `tsift explain <symbol>` — callers, callees, community context
- `tsift graph <symbol> --callers` / `--callees` — call graph navigation
- `tsift summarize <symbol>` — cached summary (only when listed in `use:`)

Only read full source files when tsift results are insufficient.
<!-- /tsift:code-navigation -->
```

The HTML comment markers enable idempotent updates without parsing markdown structure.

### Version Markers

The opening marker embeds the tsift version (`v=X.Y.Z`) that generated it. When tsift is upgraded:

- `tsift status` reports `instructions: stale` and recommends `tsift init`
- `tsift init` detects the older version marker and replaces the section with the current version's content
- Pre-versioned markers (no `v=` attribute) are treated as stale

This ensures agent sessions always use instructions matching the installed binary.

## Status (Session Health Check)

`tsift status` reports index freshness, instruction version, summary cache availability, and a machine-parseable `use:` list so the agent knows which tsift commands are worth calling this session. When the input path is a nested subdirectory, `status` first promotes it to the nearest ancestor that already owns `.tsift/` so the check reuses the existing project/workspace state. On workspace roots, it treats scoped indexes under `.tsift/indexes/<scope>/index.db` as the authoritative status surface even if a shared `.tsift/index.db` also exists, reports the contributing scopes explicitly, and surfaces configured scopes whose `index.db` is still missing so partially indexed workspaces do not masquerade as `fresh`.

```bash
tsift status            # human-readable output
tsift status --json     # structured JSON output
tsift status <path>     # check a specific codebase directory
```

### Output

Four sections: index state, instruction version, summary cache state, recommendations.

When everything is available:
```
index: fresh (last indexed 2m ago, 200 files tracked)
instructions: current (v0.1.0)
summaries: 142/200 files cached (71%)
recommendations:
  use: search, explain, graph, summarize
  run: tsift summarize --extract src/  (58 uncached files)
```

When no index exists:
```
index: missing
instructions: missing (run tsift init)
summaries: unavailable (no index)
recommendations:
  use: (none — run tsift index first)
  run: tsift init && tsift index .
```

When a workspace is indexed through scoped DBs only:
```
index: fresh (workspace, 2 scopes, last indexed 2m ago, 200 files tracked)
  scope alpha: fresh (last indexed 2m ago, 120 files tracked)
  scope beta: fresh (last indexed 1m ago, 80 files tracked)
instructions: current (v0.1.0)
summaries: none
recommendations:
  use: search, explain, graph
  run: tsift summarize --extract src/
```

When a workspace is only partially indexed:
```
index: stale (workspace, 1 indexed scope, 1 missing scope, last indexed 2m ago, 120 files tracked, 0 stale)
  scope alpha: fresh (last indexed 2m ago, 120 files tracked)
  scope beta: missing index (/repo/.tsift/indexes/beta/index.db)
instructions: current (v0.1.0)
summaries: none
recommendations:
  use: search, explain, graph
  run: tsift init --workspace && tsift index --workspace .  (1 missing scope)
```

### JSON Schema

```json
{
  "index": {
    "state": "fresh|stale|missing",
    "total_files": N,
    "stale_files": N,
    "last_indexed_secs_ago": N,
    "workspace_scopes": [
      {
        "scope": "alpha",
        "db_path": "/repo/.tsift/indexes/alpha/index.db",
        "total_files": N,
        "stale_files": N,
        "last_indexed_secs_ago": N
      }
    ],
    "missing_scopes": [
      {
        "scope": "beta",
        "db_path": "/repo/.tsift/indexes/beta/index.db"
      }
    ]
  },
  "instructions": { "state": "current|stale|missing", "version": "0.1.0", "found": "0.0.1", "expected": "0.1.0" },
  "summaries": { "state": "available|none|unavailable", "cached_files": N, "total_indexed_files": N, "coverage_pct": N },
  "recommendations": { "use": ["search", "explain", ...], "run": "tsift index ." }
}
```

### Recommendation Logic

When instructions are stale or missing, `tsift init` is prepended to the `run:` recommendation. Workspace roots use `tsift init --workspace` and `tsift index --workspace .` for their rebuild path.

| Index | Summaries | `use:` | `run:` |
|-------|-----------|--------|--------|
| missing | — | (none) | `tsift index .` |
| stale | — | search, explain, graph | `tsift index .` |
| fresh | none | search, explain, graph | `tsift summarize --extract src/` |
| fresh | partial | search, explain, graph, summarize | `tsift summarize --extract src/` |
| fresh | complete | search, explain, graph, summarize | (none) |

## Summarize (Cached LLM Analysis)

`tsift summarize` provides token-efficient access to pre-computed LLM analysis. Pay once for extraction, query free thereafter.

```bash
tsift summarize <symbol>            # show cached summary for a symbol
tsift summarize --file <path>       # show cached summary for a file/module
tsift summarize --extract <path>    # run LLM extraction on path (batch; relative path resolves against --path, or that file's parent directory)
tsift summarize --extract --diff    # re-extract only git-changed files within the requested path
tsift summarize --stats             # cache hit rate, staleness, token savings
tsift summarize --json              # structured output
```

### Architecture

```
tsift summarize
├── extract (one-time, per file content hash)
│   ├── reads source + AST symbols from index.db
│   ├── calls Anthropic batch API (haiku for cost; non-2xx responses fail closed before content parsing)
│   ├── replaces each file's cached rows in one SQLite transaction
│   └── stores: entities, relationships, summaries → summaries.db
├── query (instant, local SQLite)
│   ├── by symbol name → summary + relationships + community context
│   ├── by file path → module-level summary + exported entities
│   └── by concept → cross-file entity matches
└── invalidation
    ├── cache key: blake3(file_content) + symbol_name
    ├── --diff mode: only re-extracts tracked changes plus untracked files within the requested extract scope after that scope is canonicalized / lexically normalized, and treats unborn HEAD as untracked-only
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

1. Collect target files (from path arg or `--diff` against `git diff --name-only`; unborn HEAD falls back to untracked files only)
2. Claim the coarse `summaries.lock` sidecar so only one extractor mutates a cache at a time
3. For each file, load source + symbols from `index.db`
4. Build extraction prompt: source snippet + symbol list + "extract entities, relationships, 2-sentence summary"
5. Submit via Anthropic batch API (haiku-class model, 50% cost vs synchronous)
6. On batch completion, parse responses and insert/update `summaries.db`
7. Report: files processed, entities found, tokens spent, estimated savings

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

The hook resolves the git root first, then runs `tsift index --check --exit-code <root>` silently on every prompt. If the repo root has `.gitmodules`, it automatically switches to `tsift index --check --exit-code --workspace <root>` so one root hook covers initialized submodules. When the index is stale, it runs the matching `tsift index ...` rebuild command. When the index is fresh, the check completes in ~50ms with no side effects.

### Search Rewrite (`PreToolUse`)

The existing `tsift-rewrite.sh` hook intercepts `rg`/`grep -r` Bash calls and silently rewrites them to `tsift search --strategy lexical`. See `~/.claude/hooks/tsift-rewrite.sh`.

### RTK Output Filtering (`PreToolUse`)

The `tsift-rewrite.sh` hook (phase 2) routes verbose tsift commands through RTK for output capping when RTK is installed. Commands routed: `communities`, `explain`, `graph`, `index`, `search`. Non-verbose commands (`status`, `init`, `route`, `sql`) pass through unchanged.

RTK TOML filters at `~/.config/rtk/filters.toml` define per-command caps:

| Command | Filter | Effect |
|---------|--------|--------|
| `tsift communities` | `max_lines: 80` | Caps member lists (raw: 600+ lines) |
| `tsift explain` | `max_lines: 40` | Caps callee/caller lists |
| `tsift graph` | `max_lines: 50` | Caps edge lists |
| `tsift index` | `max_lines: 30` | Caps file change lists (raw: up to 14K+ lines) |
| `tsift search` | `strip "Strategy:" line, max_lines: 50` | Strips metadata, caps results |

All filters also strip ANSI codes and blank lines. The `--compact` and `--pretty` global flag variants are matched.

**Interaction with `--quiet`:** the `index` filter is a safety net for unqualified `tsift index` calls. When `--quiet` or `--exit-code` is passed, the binary already suppresses verbose output, making the RTK filter a no-op.

## What NOT to build

- Visualization (Mermaid, HTML) — leave to graphify
- Full LSP-level type inference — diminishing returns
- Embedding model hosting — use external API or lightweight local model (all-MiniLM-L6-v2)
- Dynamic grammar loading (until binary size exceeds ~50MB)
- Live LLM calls at query time in `tsift summarize` — extraction is batch-only
