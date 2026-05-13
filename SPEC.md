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
tsift --envelope explain <symbol> --budget normal # bounded agent preview
tsift edit < edits.json         # staged multi-file search/replace batch
tsift audit                     # scan installed skills, check health
tsift audit --manifest <file>   # compare against expected skill list
tsift summarize <symbol>        # cached LLM summary for a symbol
tsift summarize --extract <path>  # batch LLM extraction (one-time; relative path resolves against --path, workspace files use the matching scoped index)
tsift summarize --extract --diff  # re-extract only git-changed files within the requested path
tsift diff-digest [path]        # bounded worktree diff digest
tsift diff-digest --cached .    # bounded staged-index diff digest
tsift diff-digest --revision HEAD . # bounded single-revision/history digest
tsift --envelope context-pack tasks/software/tsift.md --test-input test.log --log-input build.log
tsift test-digest --path . < test.log  # bounded test-output digest from stdin or --input
tsift metric-digest < runs.json  # repeated metric-run digest: deltas, improvements, news-ready table
tsift dci-benchmark --fixture fixtures/dci-search-benchmark.json  # recorded multi-hop DCI search comparison
tsift workflow search          # handle-preserving search/explain/summarize/digest recipe
tsift log-digest --path . < build.log  # bounded verbose-log digest from stdin or --input
tsift session-digest --path . < session.md  # session transcript digest: prompt targets, commands, touched files/symbols, failures, closeout
tsift session-cost < session.jsonl  # token/runtime cost digest: prompt totals, cache ratios, large-turn outliers, restart churn
tsift --envelope session-review tasks/software/tsift.md --budget normal
tsift --envelope session-review --next-context tasks/software/tsift.md --budget normal
tsift search <query>            # lexical by default; gains AST-aware ranking when index exists
tsift search --exact <query>    # literal text lookup via `rg -F`
tsift search --autoindex <query> # explicit compatibility flag: build/rebuild before search
tsift search --scope <submod>   # restrict to one submodule's index + lexical root
tsift status --fix              # refresh stale/missing indexes and tsift instructions, then report status
tsift index --submodule <submod> # unknown/ambiguous workspace scopes fail closed
tsift search --strategy hybrid  # opt-in to slower hybrid BM25 + vector search
tsift search --timeout 60       # custom timeout in seconds (default: 30, 0 = no timeout)
tsift --compact search <query>  # terse human output across commands
```

`tsift summarize --stats`, `tsift summarize <symbol>`, and `tsift summarize --file <path>` are read-only cache queries: they fail closed when `.tsift/summaries.db` is absent, never create the summary cache as a side effect, and retry against a snapshot copy when a live SQLite lock wedges the cache. In WAL mode that snapshot copy includes the sibling `-wal` / `-shm` sidecars instead of copying only the main `.db` file, so read-only fallbacks keep the same committed live state the writer was using. `--path` first resolves through the nearest ancestor `.tsift` project/workspace root, so nested directories reuse the shared summary cache instead of creating shadow caches; `summarize --file` also normalizes equivalent path spellings back to the canonical root-relative cache key, so `src/lib.rs`, `./src/lib.rs`, nested relative spellings that point at the same file, and absolute paths routed through a symlinked checkout all hit the same cached row. Summary cache rows store that root-relative key with `/` separators even on Windows, and read/delete/currentness checks also tolerate legacy `\` rows until they are rewritten. `summarize --stats` reports stale cached files when the source file is missing, when the live blake3 hash no longer matches the cached `content_hash`, and when a cached key is absolute or lexically escapes the project root (`../...`); those out-of-root cache keys count as stale/corrupt and are never opened from the filesystem. If a cached file still exists but cannot be read during stats collection, tsift counts that row as stale, completes the report, and emits a warning instead of aborting the whole command. During `--extract`, relative extract paths resolve against the caller's `--path` anchor (or that file's parent directory), then canonicalize when possible and otherwise collapse lexical `.` / `..` segments before diff filtering, stale-row pruning, and cache-key derivation, while still reusing the ancestor project's shared summary cache. tsift claims an exclusive sibling `summaries.lock` sidecar before it deletes stale rows, rechecks content hashes, or calls the LLM so concurrent extractors fail fast instead of duplicating API spend, full re-extracts prune cached summary rows for files that no longer exist inside the requested extract scope even when that scope is now empty, workspace files resolve symbol context against the matching scoped `index.db`, symbol preload uses exact normalized file-path matches so duplicate `src/lib.rs`-style paths across scopes do not bleed into each other, symbol preload reuses the same busy-timeout plus snapshot fallback path as other read-only index consumers when a live lock is present, and `--diff` includes untracked files within the requested extract scope while deleting cached summary rows for tracked files that were removed from that scope, including the old side of `git mv` renames; on an unborn `HEAD`, `--diff` degrades to untracked-only extraction instead of failing on `git diff ... HEAD`. `tsift status` computes summary coverage against live indexed files only, so stale summary rows for deleted files do not over-report cache coverage, and it surfaces summary-cache recovery diagnostics when it had to degrade off the live database.

`tsift edit` now stages each rewritten file beside its target and only swaps the batch into place after every edit validates and every staged file is ready. If any later swap fails, tsift restores earlier files before returning an error instead of leaving a partially-written batch behind.

## Search Stale Precheck + Timeout

`tsift search` now performs a cheap freshness precheck before it calls the sift engine. If an existing local index is stale, search fails fast instead of spending up to 30 seconds in the lexical engine first.

Default behavior:

- fresh index: search proceeds normally
- stale index: search exits non-zero immediately and tells the user to run `tsift index ...`
- missing index: search still proceeds, but symbol ranking stays unavailable until the project is indexed

Opt-in recovery:

- `tsift search --autoindex ...` mirrors the hook behavior for unhooked sessions: if the local or scoped index is missing or stale, tsift incrementally builds it before searching
- if that autoindex pass only loses the coarse `index.lock` race to another live tsift writer, search now degrades instead of failing closed: stale indexes continue with the current read-only index snapshot, missing indexes fall back to exact live-file search, and stderr includes one concise retry hint for fresh symbol/index results after the writer finishes
- `tsift search --scope <submod> --autoindex ...` rebuilds only that submodule's index
- `tsift search --federated --autoindex ...` rebuilds stale/missing federated submodule indexes before aggregating symbol hits, and its lexical/vector/hybrid sift pass only searches the same federated scope roots instead of the whole workspace
- `tsift search --scope <submod> ...` now fails closed when the named submodule does not exist, and reports the available scope ids instead of silently searching the workspace root
- `tsift index --submodule <submod> ...` now fails closed on that same unknown or ambiguous selector set, instead of indexing `root/<submod>` into an unreachable scoped DB
- when duplicate submodules share the same trailing directory name, leaf-name selectors fail closed as ambiguous and the full `.gitmodules` path becomes the required scope id
- `tsift status`, `tsift search`, `tsift index`, `tsift locks`, `graph`, `communities`, `path`, and `explain` now resolve nested input paths against the nearest ancestor project/workspace root (`.tsift/` or workspace `.gitmodules`), so subdirectory invocations reuse the intended project/workspace indexes instead of creating nested `.tsift/index.db` state or inspecting synthetic nested lock files
- when a nested workspace path already falls under exactly one submodule source root, `tsift search`, `tsift locks`, `graph`, `communities`, `path`, and `explain` now infer that scoped index automatically instead of requiring a redundant `--scope <scope>` selector
- workspace roots that only have scoped `.tsift/indexes/<scope>/index.db` files now make plain `tsift search` fail closed until the caller picks `--scope <scope>` or `--federated`, instead of auto-creating a second shared root index layout
- workspace roots that only have scoped `.tsift/indexes/<scope>/index.db` files now make `graph`, `communities`, `path`, and `explain` fail closed until the caller picks `--scope <scope>`, instead of surfacing a misleading missing-root-index error
- `tsift search` symbol-hit reads now reopen `index.db` through the same resilient read-only helper used by other index consumers, so a live SQLite lock that appears after the stale-index precheck still falls back to a snapshot copy instead of bubbling a raw SQLite lock error
- writable index updates now claim an OS-backed exclusive lock on the sibling `index.lock` sidecar first, so concurrent `tsift index` / `tsift search --autoindex` writers fail fast with a tsift-owned error instead of surfacing raw SQLite lock contention or PID-recycling false positives
- read-only graph queries (`graph`, `communities`, `path`, `explain`) open `index.db` without taking that writer-side `index.lock`, and when a live SQLite lock wedges the database they retry against a snapshot copy, including WAL sidecars when present, so diagnostic and graph traversal commands stay available
- writable `index.db` opens also set `PRAGMA wal_autocheckpoint=256`, so normal tsift write traffic checkpoints the WAL on an explicit budget instead of leaving it entirely to SQLite defaults
- non-fatal source-read / symbol-extraction / call-extraction failures now emit warnings instead of being silently swallowed, and those warnings are carried in `IndexSummary` for JSON consumers

`tsift search` still wraps the sift engine call in a 30-second timeout (configurable via `--timeout`). Timed searches now run in an internal helper process so a timeout kills the underlying sift work instead of leaving a detached worker thread behind. The timeout remains a backstop for genuinely slow lexical searches or for sessions that reach search without a usable index.

Both the in-process lexical path and the timed `__search-worker` helper now point sift at a stable `.tsift/search-cache` directory under the resolved project/workspace root. That keeps corpus/BM25 artifacts reusable across repeated searches, including scoped and federated queries that execute from subpaths but still belong to the same root-owned `.tsift/` state.

`tsift search --exact` (and `--strategy exact`) bypasses that lexical/index precheck entirely and executes a literal `rg -F` scan instead. Plain `tsift search <query>` also auto-promotes single-token identifier/path-like queries such as `claudescore-3`, `alpha_helper`, `src/main.rs`, and `crate::module` to that exact backend by default. That path keeps rg-style lookups fast, works even when the symbol index is stale or missing, and does not require a shared root `.tsift/index.db` in workspaces that only maintain scoped indexes.

When an exact or otherwise high-hit search returns repeated matches from the same file, the default human output now collapses those repeated line hits into one file-level entry with hit counts first and only a couple of representative snippets after that. That keeps broad literal lookups usable without relying on external line truncation alone.

Because that auto-exact routing closes the main literal-lookup gap after the stable search cache work, tsift still defers any native content/FTS table inside `.tsift/index.db`. Broad prose retrieval remains sift's job; exact content lookups stay on ripgrep unless real usage proves that rg-backed exact search leaves an important gap.

`tsift explain` keeps the full JSON/tabular edge list, but the default human-readable caller/callee sections now collapse dense same-file edge sets into grouped file rows with counts. That reduces token volume for highly-connected symbols while preserving the concrete caller/callee names in the grouped summary.

## Handle-Preserving Search Workflow

`tsift workflow search` prints a composable recipe for agents that need to move from literal lookup to broader retrieval without losing stable handles. The JSON and envelope forms (`tsift --envelope workflow search`, `tsift workflow search --json`) list ordered steps for:

- exact anchors: `tsift --envelope search "<literal>" --exact --path . --budget normal`
- semantic broadening: `tsift --envelope search "<concept>" --strategy hybrid --path . --budget normal`
- graph expansion: `tsift --envelope explain "<symbol>" --path . --budget normal`
- summary reads: `tsift summarize "<symbol>" --path . --json`
- digest expansion: `tsift --envelope context-pack <path> --test-input test.log --log-input build.log --budget normal`

The contract is to keep every emitted handle with its originating command, query, path, and strategy, then use each result's `expand`, `follow_up`, or `resume_commands` field for the next command while citing the parent handle. Search previews preserve `sfam-*` and `shit-*` handles, explain previews preserve `edef-*`, `ecall-*`, and `eces-*` handles, and digest/context-pack outputs preserve artifact and touched-symbol handles across diff, test, log, and session expansions.

When an index is present, the AST symbol-ranking prepass is now bounded: SQLite only pulls exact-name rows and overlapping-tag candidates, orders them by exact/tag overlap, and caps that candidate scan to the requested search `--limit` instead of loading the full `symbols` table into memory first.

On stale existing indexes, search exits early with a message like:
```
tsift search aborted: index is stale (51 files). Run `tsift index .` or re-run with `--autoindex`.
```

If the sift engine itself still times out while the search target is fresh, search exits with a non-zero code and prints:
```
tsift search timed out after 30s (strategy: lexical). The search root looks fresh, so reindexing is unlikely to help. Re-run with `--timeout 0` to disable the timeout, narrow `--path` / `--scope`, or try a different strategy.
```

If the timeout re-check finds that the index became stale or disappeared while the worker was running, the timeout error switches back to a concrete rebuild instruction for that target instead of the fresh-root hint.

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
- `path`, `status`, `audit`, `summarize`, `diff-digest`, `test-digest`, `log-digest`, `lint`, `sql`, and `index` switch to denser summary-oriented layouts

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

## Budget-Aware Preview Profiles

`tsift search`, `tsift explain`, `tsift session-review`, and `tsift context-pack` expose preview budgets for agent-facing turns that need bounded follow-up surfaces instead of full prose dumps:

```bash
tsift search "alpha_helper" --budget small
tsift explain alpha_helper --budget normal
tsift session-review tasks/software/tsift.md --budget deep --json
tsift token-savings --fixture fixtures/tsift-token-savings.json --fail-under --json
```

Behavior:

- `--budget <small|normal|deep|auto>` applies named presets. `small` uses 3 items / 120 bytes, `normal` uses 5 items / 160 bytes, and `deep` uses 10 items / 240 bytes.
- `--budget auto` chooses a preset from `TSIFT_CONTEXT_WINDOW`, `CODEX_CONTEXT_WINDOW`, or `CLAUDE_CONTEXT_WINDOW` when one is present: windows at or below 64k use `small`, windows at or above 200k use `deep`, and the fallback is `normal`.
- `tsift --envelope` turns on the adaptive budget by default for these preview-capable commands. Explicit `--budget`, `--max-items`, or `--max-bytes` still wins.
- `--max-items <n>` switches the command into preview mode and caps repeated result groups to `n` items per section.
- `--max-bytes <n>` truncates long preview fields (snippets, messages, paths, labels) to `n` bytes with an ellipsis.
- Preview mode emits deterministic expansion handles plus a concrete follow-up `expand` command for each preview item, so callers can request a narrower rerun without paying for the full original response. Follow-up digest commands use an independent 4-command floor and are not byte-truncated, so `small` remains compact without hiding the `session-review`, `diff-digest`, `test-digest`, and `log-digest` commands needed to resume from a compact handoff.
- Before lexical or hybrid fallback, `tsift search` normalizes free-text agent queries through the `tagpath` query API, so phrases like `get user profile`, `profile user get`, and identifier-shaped terms such as `getUserProfile` resolve against the same canonical symbol-tag stream.
- Symbol-bearing preview items expose a canonical `tag_alias` derived from the `tagpath` family API (for example `alpha/helper`), so search, explain, session-review, and context-pack use one shared family model across surface spellings.
- `context-pack` loads tagpath ontology docs from `.naming/tags/*.md` when present and attaches compact `ontology_refs` to visible symbol refs and summary refs. Each ref carries a stable handle, canonical tag, markdown path, and optional title/domain metadata, while deliberately omitting ontology prose so agents can expand the tag document by path only when needed.
- When search preview mode sees repeated symbol hits that collapse to the same canonical `tag_alias`, it emits one family summary row with match/file counts plus a follow-up `expand` command keyed to that canonical tag family instead of repeating every surface spelling inline.
- When a search preview looks too broad for safe fan-out, the report includes a `scale_guard` with `high-hit` or `corpus-size` level, explicit corpus/tool-budget signals, and concrete `narrow_commands` to run before dispatching parallel agents. Envelope `follow_up` lists those narrowing commands before ordinary item expansion commands.
- JSON/terse/schema output in preview mode returns the same bounded preview report instead of the full raw payload; without these flags, the existing output formats remain unchanged.

`tsift token-savings --fixture <path>` is a CI-friendly report surface for preview compression contracts. The fixture lists per-command cases with raw symbol rows, compact tagpath families, and minimum savings thresholds; session-review cases can include raw `prompt_targets`, `sessions`, `commands`, `touched_files`, `touched_symbols`, `failures`, `guardrails`, and `largest_turns`, while context-pack cases can include raw `next_context`, `diff`, `test`, and `log` input rows. That keeps the benchmark focused on the real transcript and handoff sections that dominate prompt volume, not only symbol-family compression. tsift serializes the raw rows and the compact envelope rows, then reports byte deltas, estimated token deltas using `ceil(utf8_bytes / 4)`, savings percentages, and pass/fail status per command. `--json` emits the report as structured data, `--fail-under` exits non-zero when any case misses its fixture threshold, and `tsift --envelope token-savings ...` wraps the same report in the common summary envelope.

`tests/exit_code.rs` runs the compiled `tsift token-savings --fixture ../tagpath/fixtures/tsift-token-savings.json --fail-under --json` path against tagpath's shared fixture and locks the current preview contract to `search`, `explain`, `session-review`, and `context-pack`, including the context-pack fail-under threshold for compact handoff previews. It also runs `fixtures/real-session-token-savings.json`, a tsift-owned benchmark derived from recent tsift/agent-doc transcripts, so `session-review` and `context-pack` keep proving large savings on realistic prompt-target, transcript, diff, test, build, install, and push handoff rows. The current real-session fixture reports 85.5% savings overall (about 3.4k estimated tokens saved) while retaining the resumable follow-up command surface.

## Structured Envelopes

`tsift --envelope` is a global JSON-mode wrapper for agent-facing preview and handoff commands. It currently applies to `search`, `explain`, `session-review`, and `context-pack`, and it implies `--json`.

Example:

```bash
tsift --envelope search "alpha_helper" --budget small
tsift --envelope session-review tasks/software/tsift.md --next-context --budget normal
tsift --envelope context-pack tasks/software/tsift.md --test-input target/test.log --budget auto
```

Envelope shape:

- `tool`: command name (`search`, `explain`, `session-review`, `context-pack`)
- `view`: report shape such as `preview`, `report`, `next-context`, or `handoff`
- `summary`: terse display payload with `text` plus `metrics[{label,value}]`
- `truncated`: whether the wrapped report is budget-trimmed
- `follow_up`: concrete rerun or expansion commands callers can surface directly
- `report`: the existing command-specific JSON payload

Preview reports keep their item-level `handle` + `expand` fields inside `report`, so clients can render a top summary from the envelope and then request narrower follow-up expansion without falling back to prose-heavy defaults.

Search preview reports may also include `report.scale_guard`. Clients should surface that warning prominently and prefer the guard's `narrow_commands` before launching independent search/explain/summarize work, because those commands encode the result-count, corpus-size, and preview-budget context that made the original query risky.

### Command/Test-Run Envelopes

`tsift --envelope __digest-runner ... --json` now wraps command-execution digests in a summary-first envelope for `test` and `log` runs.

Behavior:

- The outer envelope stays terse (`tool: "digest-runner"`, `view: "test-run"` or `view: "command-run"`) and surfaces only the summary metrics callers need first, such as runner, exit code, failure count, or signal count.
- The inner `report` carries command metadata plus the existing `test-digest` or `log-digest` payload under `digest`. `report.command` remains the caller's original command, `report.executed_command` records the actual command tsift ran, and `report.filter` records delegated compression metadata when present.
- When `rtk` is installed and `rtk rewrite <command>` supports the wrapped command family, digest-runner executes the RTK-filtered command and wraps that compact output in the same tsift envelope/artifact metadata. Unsupported commands or missing RTK fall back to tsift's built-in capture/digest path.
- When captured stdout/stderr is non-empty, tsift persists it under `.tsift/artifacts/` and returns `report.artifact = {handle, path, bytes, lines, expand}`.
- `handle` is stable for the captured transcript body and command identity, so clients can reference the artifact without inlining the raw output into context.
- `expand` is a concrete replay command (`tsift test-digest ... --input <artifact>` or `tsift log-digest ... --input <artifact>`) that rehydrates the bounded digest from the stored artifact only when the caller explicitly wants details.
- Successful/green runs therefore stay summary-first by default: callers can report the pass/build outcome from the envelope and keep the raw transcript behind the artifact handle instead of replaying it into the turn.

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

**Locking:** `tsift communities` is a read-only graph query. It opens the existing `index.db` without acquiring the writer-side `index.lock`, and if a live SQLite lock temporarily blocks reads it retries against a snapshot copy, including WAL sidecars when present, so the command remains available.

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
- `tsift lint` opens discovered `index.db` files through the shared read-only path with snapshot fallback for live SQLite locks, including WAL sidecars when present, so lint stays available while a live writer has the index locked.

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

`tsift init` ensures the Code Navigation section is present in `AGENTS.md` for Codex-style harnesses and mirrors it into `CLAUDE.md` when that file exists, so local agent sessions prefer envelope previews plus artifact-backed digest surfaces over raw file reads, diffs, and verbose logs.

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
8. The injected Code Navigation section explicitly tells harnesses to switch to the owning repo or submodule root before running tsift/build/test commands, so submodule work does not inherit the wider superproject instruction surface by accident.
9. The injected section also steers harnesses toward envelope-backed `search`, `explain`, `session-review`, `context-pack`, and digest-runner artifacts instead of raw transcript replays, `git diff/show/log` patch dumps, or verbose build/test output reads.
10. The injected section tells agents to run the local default suite with `make check`, then check the latest GitHub Actions CI run with `gh run list --workflow CI --limit 1`; the wider ignored deterministic simulation corpus is a CI-owned `make ci-full` concern and must be fixed before the work is complete if CI reports failures.

### Injected Section

```markdown
<!-- tsift:code-navigation v=0.1.42 -->
## Code Navigation

Run `tsift status` at session start from the owning repo root. If the task or file lives under a git submodule (for example `src/tsift/...`), switch to that submodule root first so the harness loads the narrower local instructions and repo state instead of the superproject root. If status prints a `run:` recommendation for stale or missing tsift state, run `tsift status --fix` before relying on tsift results; when the harness cannot perform write commands, ask the user to run the printed command instead. Codex projects can install a prompt-time auto-reindex hook with `tsift init --codex`.

Use the commands listed in its `use:` output:
- `tsift --envelope search <query> --budget normal` — AST-aware hybrid search preview (prefer over grep/rg)
- `tsift --envelope explain <symbol> --budget normal` — callers, callees, community preview
- `tsift graph <symbol> --callers` / `--callees` — call graph navigation
- `tsift summarize <symbol>` — cached summary (only when listed in `use:`)
- `tsift workflow search` — ordered exact/search/explain/summarize/digest recipe that preserves result handles across expansions

When a search envelope includes `report.scale_guard`, run one of its `narrow_commands` before dispatching parallel agents. The guard means the original result set or corpus is broad enough that fan-out should start from a narrower cited handle, path, or exact query.

Prefer bounded digest commands over raw transcript, diff, and verbose-log reads:
- `tsift --envelope session-review <path> --next-context --budget normal` or `tsift --envelope context-pack <path> --budget normal` instead of replaying long session docs, JSONL transcripts, or agent-doc runtime logs with `cat`, `tail`, or `sed`.
- `tsift diff-digest [path]` (`--cached`, `--revision <rev>`) instead of `git diff`, `git show`, or patch-style `git log`.
- `tsift --envelope __digest-runner --kind test --path . --shell-command '<test command>'` / `tsift --envelope __digest-runner --kind log --path . --shell-command '<build command>'` for noisy test/build/install output, or let the rewrite/hooks create those artifact-backed envelopes for `cargo test`, `pytest`, and verbose cargo commands.
- If RTK is installed, digest-runner delegates supported generic command families through `rtk rewrite` and records the chosen compact filter in `report.filter` while preserving tsift artifact handles.
- If your harness does not support Claude-style `PreToolUse` hooks, run `tsift rewrite --run '<command>'` to execute the same envelope-first, artifact-backed tsift equivalent manually.

For local verification, run `make check` before committing. CI owns the wider ignored deterministic simulation corpus via `make ci-full`; after local changes, check the latest GitHub Actions CI run with `gh run list --workflow CI --limit 1` and fix any failing tests before calling the work complete.

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
Release-bump regressions are covered through the compiled CLI path: a stale Code Navigation marker from the previous binary version must be rewritten by `tsift status --fix --json`, and the final JSON report must show `instructions.state=current` for the installed version.

## Status (Session Health Check)

`tsift status` reports index freshness, instruction version, summary cache availability, and a machine-parseable `use:` list so the agent knows which tsift commands are worth calling this session. When the input path is a nested subdirectory, `status` first promotes it to the nearest ancestor that already owns `.tsift/` so the check reuses the existing project/workspace state, but it stops at a nested git root before considering parent `.tsift/` directories and ignores ambient system-temp-root project markers for child temp dirs so unrelated temp or parent workspaces cannot capture a child repo. On workspace roots, it treats scoped indexes under `.tsift/indexes/<scope>/index.db` as the authoritative status surface even if a shared `.tsift/index.db` also exists. If one or more configured workspace scopes are present on disk but their scoped `index.db` files are missing, the CLI auto-builds just those missing scoped indexes before it prints the final status so a partially initialized workspace does not stay stuck at `index: missing` / `stale` after a successful status pass. `tsift status --fix` additionally applies the safe local fixes behind the `run:` recommendation: refresh stale or missing indexes, rebuild all existing workspace scopes when the workspace index is stale, refresh stale/missing Code Navigation instructions via `tsift init`, and then print the final status. When status recommends `tsift summarize --extract ...`, that extract scope is derived from the indexed layout: it uses the common indexed root (for example `src/` when every tracked file or scope lives under `src/`) and falls back to `.` when the indexed files span the project root or multiple unrelated workspace roots.

```bash
tsift status            # human-readable output
tsift status --json     # structured JSON output
tsift status <path>     # check a specific codebase directory
tsift status --fix      # apply safe local index/instruction refreshes before reporting
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

When instructions are stale or missing, `tsift init` is prepended to the `run:` recommendation. Workspace roots use `tsift init --workspace` and `tsift index --workspace .` for their rebuild path. Fresh-index summarize recommendations derive the `--extract` target from the indexed layout instead of assuming `src/`.

| Index | Summaries | `use:` | `run:` |
|-------|-----------|--------|--------|
| missing | — | (none) | `tsift index .` |
| stale | — | search, explain, graph | `tsift index .` |
| fresh | none | search, explain, graph | `tsift summarize --extract <common indexed root>` |
| fresh | partial | search, explain, graph, summarize | `tsift summarize --extract <common indexed root>` |
| fresh | complete | search, explain, graph, summarize | (none) |

## Summarize (Cached LLM Analysis)

`tsift summarize` provides token-efficient access to pre-computed LLM analysis. Pay once for extraction, query free thereafter.

```bash
tsift summarize <symbol>            # show cached summary for a symbol
tsift summarize --file <path>       # show cached summary for a file/module
tsift summarize --extract <path>    # run LLM extraction on path (batch; relative path resolves against --path, or that file's parent directory)
tsift summarize --extract --diff    # re-extract only git-changed files within the requested path
tsift summarize --stats             # summary totals, stale-file count, token savings
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

`--stats` reports: total summaries, cached files, stale files, and estimated tokens saved across sessions.

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

## Diff Digest

`tsift diff-digest [path]` turns worktree, staged, or single-revision diffs into a bounded, code-aware report for agent context.

```bash
tsift diff-digest .        # current repo root
tsift diff-digest --cached . # staged index against HEAD
tsift diff-digest --revision HEAD . # HEAD commit against its first parent
tsift diff-digest --json . # structured output
```

Behavior:

1. In default mode, collect tracked changes from `HEAD` plus untracked files and compare `HEAD` to the working tree. With `--cached`, compare the staged index to `HEAD`. With `--revision <rev>`, compare that single revision to its first parent (or to the empty tree for a root commit).
2. Parse both snapshots directly with tree-sitter when the file language is supported.
3. Emit changed-file status, touched symbols, up to two current cached summary snippets when `summaries.db` matches the compared snapshot, and added/removed call edges.

`diff-digest` intentionally does not require a fresh `index.db`. It reads the compared snapshots directly so unindexed working-tree edits, staged-only content, and historical commit review all stay bounded without mutating the index. Summary lookups stay read-only and degrade to `missing`, `stale`, or `unavailable` instead of mutating the cache.

## Test Digest

`tsift test-digest` turns captured test runner output into a bounded failure report for agent context.

```bash
cargo test 2>&1 | tsift test-digest --path .
tsift test-digest --runner pytest --input .pytest-failures.log --json
```

Behavior:

1. Read captured test output from stdin by default, or from `--input <file>`.
2. Auto-detect `cargo` and `pytest` output formats unless `--runner` forces one parser.
3. Group duplicate failures by file/line/message, preserve the failing test names, and keep the first assertion/error message instead of the full transcript noise.
4. When `.tsift/summaries.db` already has current rows for an anchored file, include up to two cached summary snippets; otherwise report `missing`, `stale`, or `unavailable` without mutating the cache.

`test-digest` is intentionally transcript-only. It does not execute the test runner itself, and it keeps summary enrichment read-only so digesting noisy output never contends with `tsift summarize --extract`.

## Metric Digest

`tsift metric-digest` turns repeated metric-run histories into bounded deltas for agent context and news updates.

```bash
tsift metric-digest --input runs.json
tsift metric-digest --baseline yesterday.json --input today.json --metric session_mae --metric composite_score
cat benchmark-runs.ndjson | tsift metric-digest --lower-is-better session_mae --higher-is-better composite_score
```

Accepted input shapes:

- a single JSON object with a `metrics` map
- a JSON object with `runs: [...]`
- a JSON array of run objects
- NDJSON with one run object per line

Each run object may include `label`, `id`, and `timestamp`, plus either `metrics: {key: number}` or inline numeric metric fields.

Behavior:

1. Read run history from stdin by default, or from `--input <file>`.
2. Compare the latest input run against `--baseline <file>` when present; otherwise compare it against the previous run in the same history.
3. Infer common metric directions automatically (`mae`, `latency`, `cost`, `error` prefer lower; `score`, `accuracy`, `pass`, `throughput` prefer higher) and allow explicit `--lower-is-better` / `--higher-is-better` overrides.
4. Emit bounded per-metric deltas, top improvements/regressions, and a markdown-ready history table suitable for session notes or news updates.

`metric-digest` is intentionally schema-light. It does not execute the underlying benchmark/test/perf workflow, and it avoids hard-coding session-share-specific parsers so different run producers can feed the same digest surface.

## DCI Benchmark

`tsift dci-benchmark --fixture <path>` summarizes recorded Direct Corpus Interaction search runs for multi-hop repo/code tasks. The benchmark fixture compares the three strategy lanes tsift cares about after the DCI paper review:

- `exact_chained_rg`: literal `rg -F` / `tsift search --exact` narrowing with local context expansion
- `lexical_bm25`: the default sift/BM25 search path
- `hybrid`: slower BM25 + vector-assisted search

Each task records whether the strategy localized the intended edit/review target plus `tool_calls`, `latency_ms`, and `estimated_tokens`. The report aggregates localization rate, average tool calls, average latency, and average token budget per strategy, then ranks strategies by localization first and agent budget second. Missing expected lanes are warnings, not hard failures, so partial experiments can still be digested while making gaps visible.

The checked-in `fixtures/dci-search-benchmark.json` is a seed benchmark for tsift's own multi-hop workflows: rewrite/digest routing, summary-cache lock fallback, and workspace scope fail-closed localization. It is intentionally recorded-run based rather than a live runner, so CI stays deterministic and hybrid/vector model downloads do not gate normal verification. Live benchmark scripts can append new task records and use `tsift dci-benchmark --json` as the stable summarizer.

## Deterministic SimWorld

`src/sim_world.rs` provides a tsift-local deterministic simulation harness for high-risk agent workflow states that should not require live tmux or long CLI matrices. The fast corpus runs in normal `cargo test`; the wider ignored corpus runs through `make ci-full` in GitHub Actions.

The model currently covers:

- session prompt-target extraction, including live exchange prompts versus copied instruction/frontmatter/archive ballast;
- rewrite routing for long session reads, short passthrough reads, test/build digest-runner wrappers, diff-digest routing, and shell metacharacter passthrough;
- status recommendation transitions for missing, stale, and current Code Navigation instructions.

Coverage counters are explicit and fail closed when a named edge class disappears from the corpus. This mirrors the agent-doc pattern of replacing expensive live tmux edge sweeps with deterministic model coverage first, while keeping wider ignored simulation budgets in CI rather than the local development loop.

## Log Digest

`tsift log-digest` turns captured verbose stdout/stderr into a bounded transcript digest for agent context.

```bash
cargo build 2>&1 | tsift log-digest --path .
tsift log-digest --input target/build.log --json
```

Behavior:

1. Read captured log output from stdin by default, or from `--input <file>`.
2. Collapse repeated lines, group warning/error signal lines, and count repeated stack blocks so noisy transcripts stay bounded.
3. Extract file anchors and symbol-like tokens from the transcript for quick follow-up lookups. Agent-doc runtime-style `file=...` and `path=...` fields count as file anchors even when they do not carry line numbers; timestamped event names plus `event=...`, `pane=...`, and `session=...` fields are retained as structured symbol refs.
4. When `.tsift/summaries.db` already has current rows for anchored files or extracted symbols, include up to two cached summary snippets; otherwise report `missing`, `stale`, or `unavailable` without mutating the cache.

`log-digest` is intentionally transcript-only. It does not execute the underlying command, and it keeps summary enrichment read-only so digesting verbose output never contends with `tsift summarize --extract`.

## Session Digest

`tsift session-digest` turns long session transcripts and harness runtime logs into bounded execution evidence for agent context.

```bash
tsift session-digest --path . < tasks/software/tsift.md
tsift session-digest --source claude-jsonl --input ~/.claude/projects/foo/session.jsonl --json
tsift session-digest --source codex-jsonl --input ~/.codex/sessions/2026/05/02/rollout-....jsonl --json
tsift session-digest --source agent-doc-log --input .agent-doc/logs/tsift-v0.1.log --json
```

Accepted sources:

- markdown session documents such as `agent-doc` / Codex task files
- Claude JSONL transcripts with `message.content` text/tool blocks
- Codex JSONL transcripts with `response_item` / `event_msg` records
- `agent-doc` runtime `.log` files with session start/restart/timeout/exit events

Behavior:

1. Read captured session input from stdin by default, or from `--input <file>`.
2. Auto-detect markdown, Claude JSONL, Codex JSONL, or `agent-doc` runtime logs unless `--source markdown|claude-jsonl|codex-jsonl|agent-doc-log` forces one parser.
3. Extract bounded prompt targets, shell commands, touched file paths, symbol-like identifiers, failure lines, runtime-event churn, and closeout evidence such as verification/install/commit/push/version mentions.
4. Ignore copied harness-instruction ballast such as markdown headings, placeholder slash-command examples, and bold imperative labels so prompt/failure hotspots stay focused on actual session work.
5. Treat successful test summaries and bare section labels as non-failures: lines such as `failures:`, `No failures detected`, `test result: ok. ... 0 failed`, and `4 passed, 0 failed` must not appear in session-digest failures or session-review unresolved failures, while real panic/assertion/error/exit evidence is preserved with its command/session anchors.
6. Keep the digest transcript-only: it summarizes what happened in the session, but it does not replay tool calls or attempt to reconstruct the full conversation.

`session-digest` is intentionally conservative. It favors bounded evidence over perfect transcript reconstruction so long agent sessions can be collapsed into compact handoff or review context.

## Session Cost

`tsift session-cost` turns Claude/Codex transcript usage and `agent-doc` runtime logs into bounded cost summaries for agent context.

```bash
tsift session-cost --input ~/.claude/projects/foo/session.jsonl --json
tsift session-cost --source codex-jsonl --input ~/.codex/sessions/2026/05/02/rollout-....jsonl
tsift session-cost --source agent-doc-log --input .agent-doc/logs/tsift-v0.1.log
```

Accepted sources:

- Claude JSONL transcripts with assistant `message.usage` payloads
- Codex JSONL transcripts with `event_msg` `token_count` records
- `agent-doc` runtime `.log` files with start/restart/timeout events

Behavior:

1. Read captured transcript/log input from stdin by default, or from `--input <file>`.
2. Auto-detect Claude JSONL, Codex JSONL, or `agent-doc` runtime logs unless `--source claude-jsonl|codex-jsonl|agent-doc-log` forces one parser.
3. Normalize prompt-side totals, cached-input totals, output totals, and largest per-turn outliers so token-heavy sessions can be compared without ad hoc `jq` pipelines.
4. For `agent-doc` runtime logs, summarize bounded churn counters such as `fresh_restart`, `continue`, and `auto_trigger_timeout`, including the highest observed `restart_count`.
5. Derive bounded restart-churn families from `agent-doc` logs so the digest can call out `fresh_restart`, `auto_trigger_timeout`, ctrl-d restart loops, and quit-after-eof cycles without replaying the full raw event stream.
6. Summarize bounded loop clusters for repeated prompt bodies, repeated command bundles, and repeated closeout churn so common restart/retry patterns become explicit instead of hiding inside the top-N command/event lists.
7. Emit guardrails when the session shows obvious budget risk: oversized prompt turns, very high cached-input resend ratios, restart-loop churn, or repeated `commit_already_current` no-op closeouts. For newer `agent-doc` `document_cycle` logs, collapse repeated closeout lines to one occurrence per `(cycle, event)` before counting so retry noise does not swamp the summary. Each guardrail includes actionable compact/restart guidance.

`session-cost` is intentionally cost-focused. It does not reconstruct the full conversation or replay tool calls; it compresses token/runtime overhead into a bounded report you can paste into backlog triage, handoffs, or benchmark notes.

## Session Review

`tsift session-review` auto-discovers related Claude/Codex transcript logs plus `agent-doc` runtime logs for a document or repo path, then emits one bounded combined review.

```bash
tsift session-review tasks/software/tsift.md
tsift session-review --next-context tasks/software/tsift.md
tsift session-review src/tsift --json
```

Behavior:

1. Resolve the owning repo/submodule root for the target path.
2. For document targets, read `agent_doc_session` from frontmatter when present and use the matching `.agent-doc/logs/<session>.log` to learn historic `file=` aliases plus prior `session=` aliases before scanning other harness logs.
3. Discover related Claude sessions under `~/.claude/projects/<cwd-slug>/`, Codex sessions under `~/.codex/sessions/`, and `agent-doc` runtime logs under `<root>/.agent-doc/logs/`.
4. For directory targets, match candidate logs by cwd. For document targets, require a document-specific signal (`agent_doc_session` or a document path alias) before counting a Claude/Codex transcript; when cwd also matches, report it as supporting evidence instead of letting a shared workspace cwd count by itself. Candidate matching should use structured user/tool-input snippets rather than arbitrary transcript stdout so unrelated hook output or command dumps do not overmatch a shared workspace file name. Reuse the existing `session-digest` and `session-cost` parsers to aggregate prompt targets, commands, failures, closeout evidence, token totals, restart churn, and repeated loop clusters into one bounded report.
5. Claude/Codex transcript parsing should skip malformed JSONL lines and ignore non-conversation attachment records where possible so one bad line or hook payload does not fail the whole review.
6. Session-review inherits session-digest's instruction-ballast and successful-test-summary filtering so copied harness docs and passing test output do not dominate prompt/failure hotspots.
7. Session-review also carries forward aggregate session-cost guardrails so document-level reviews warn when token spend is mostly cached resend, restarts are looping, or closeouts are mostly no-ops. The review should also surface repeated prompt bodies, repeated command bundles, and repeated closeout churn as explicit loop-cluster summaries instead of leaving that repetition buried inside broader aggregates. When the source is an `agent-doc` runtime log, normalize `document_cycle` closeout details to `phase + event` and count them once per cycle so the review reports distinct closeout cycles instead of raw repeated retries.
8. `--next-context` emits only the bounded resumable handoff pack: active prompt targets, the last verification closeout state, touched files/symbols, unresolved failures, and the next digest commands to run instead of replaying raw session/log history. For agent-doc template documents with a live unresolved `agent:exchange` tail after the latest response boundary, prompt targets, touched files/symbols, and unresolved failures come from that tail rather than historical transcript aggregates. Frontmatter prompt presets, examples, compacted/archive summaries, completed backlog entries, resolved `### Re:` responses, repeated resolved directives, and stale/bogus paths from old matched sessions must not reappear as current handoff work. If no live document tail is available, `session-review` falls back to the bounded aggregate review fields.

`session-review` is intentionally bounded. It does not replay full conversations; it gives one cross-harness review surface so document-level session analysis stops depending on ad hoc file hunting and manual aggregation.

### `context-pack`

`tsift context-pack <path>` turns the existing bounded session/diff/test/log surfaces into one resumable handoff payload for agent turns.

Example:

```bash
tsift context-pack tasks/software/tsift.md --test-input test.log --log-input build.log --json
```

Behavior:

1. Computes `session-review --next-context` for the target document or repo path.
2. Computes the current worktree `diff-digest` for the resolved repo root.
3. Optionally inlines `test-digest` when `--test-input <file>` is provided.
4. Optionally inlines `log-digest` when `--log-input <file>` is provided.
5. Emits the follow-up digest commands needed to refresh or expand the pack without replaying raw transcripts or verbose logs.

`context-pack` is intentionally bounded by default: it emits preview-style lists plus counts rather than dumping the full underlying reports, and `--max-items` / `--max-bytes` further tighten the preview envelope for high-token-pressure turns. Its symbol-bearing preview lists keep the raw `touched_symbols` strings for compatibility while also adding compact symbol-ref objects with stable `handle` ids and canonical `tag_alias` values for `next_context`, diff previews, and log symbol references. If tagpath ontology docs exist under `.naming/tags/*.md`, `context-pack` also loads them once and attaches compact `ontology_refs` to matching symbol refs, summary refs, and the top-level pack; those refs carry handle/tag/path metadata so stable domain vocabulary can be referenced without inlining repeated prose definitions. When the underlying diff/test/log digest already found current cached summaries, the corresponding touched file, failure, signal, file-ref, and symbol/tag-alias family rows expose bounded `summary_refs` with stable handles plus `tsift summarize --file ...` or `tsift summarize <symbol>` expansion commands, so resumptions can keep summary context behind handles instead of inlining every cached summary body.

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

The existing `tsift-rewrite.sh` hook intercepts high-token shell commands and silently rewrites them to lower-context tsift flows:

- `rg ...` / `grep -r ...` → `tsift --envelope search ... --exact --budget normal`
- `git diff`, `git diff --cached`, `git show`, and simple `git log -p -1 ...` history review → `tsift diff-digest ...`
- long transcript reads (`cat`, `bat`, `head -n`, `tail -n`, `sed -n`) over recognized agent-doc markdown sessions, Claude JSONL, Codex JSONL, or `agent-doc` runtime logs → `tsift session-digest ...`, anchored to the transcript's owning repo or submodule root when the file lives under one
- `cargo test ...`, `pytest ...`, `python -m pytest ...` → `tsift --envelope __digest-runner --kind test ...`
- `cargo build ...`, `cargo check ...`, `cargo clippy ...`, `cargo install ...` → `tsift --envelope __digest-runner --kind log ...`

The digest-runner path preserves the wrapped command's original exit status while replacing raw stdout/stderr with a summary-first envelope, bounded digest, and persisted transcript artifact, so failing tests/builds still fail closed and green runs do not inline raw logs. When RTK is installed, digest-runner probes `rtk rewrite <command>` and delegates supported generic command families to RTK's compact filters before wrapping the filtered output in tsift's envelope/artifact metadata. See `~/.claude/hooks/tsift-rewrite.sh`.

Harnesses that do not expose Claude-style `PreToolUse` hooks can still reuse the same rewrite path manually via `tsift rewrite --run '<command>'`. In `--run` mode, tsift executes the rewritten command directly instead of only printing it, preserves the rewritten command's exit status, and emits the same envelope search previews and digest-runner artifact envelopes by default.

Global structured-output flags are forwarded into the rewritten tsift command and deduplicated when the rewrite already chose an envelope. That means callers can still layer `--pretty`, `--terse`, or `--schema` onto the default summary-first execution output, for example:

- `tsift --pretty rewrite --run 'cargo test --manifest-path Cargo.toml'`
- `tsift --schema rewrite --run 'cargo build --manifest-path Cargo.toml'`
- `tsift rewrite --run 'cargo install --path . --force'`

Those commands emit the same `digest-runner` JSON envelope that `tsift --envelope __digest-runner ... --json` uses internally, so agent-doc or other harnesses get bounded execution output without depending on shell-hook rewriting. If RTK is available and supports the wrapped command, `report.filter = {tool:"rtk", command:"..."}` identifies the delegated compact filter.

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

Outside the Claude hook path, `tsift rewrite --run '<command>'` provides a built-in fallback for the same bounded-output policy. Structured `--json` / `--terse` / `--schema` / `--tabular` output stays untouched; remaining human-readable passthrough output is capped only for already-tsift verbose commands that do not have an envelope/structured rewrite form.

## What NOT to build

- Visualization (Mermaid, HTML) — leave to graphify
- Full LSP-level type inference — diminishing returns
- Embedding model hosting — use external API or lightweight local model (all-MiniLM-L6-v2)
- Dynamic grammar loading (until binary size exceeds ~50MB)
- Live LLM calls at query time in `tsift summarize` — extraction is batch-only
