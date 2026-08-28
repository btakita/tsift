# tsift Spec — Subcommands, Search & Navigation

Part of the [tsift spec](../SPEC.md). See that index for the full command/spec map.

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
tsift traverse [node] [--to target] --format json|html # Graphify-style file/symbol/session/backlog traversal graph
tsift convex-sync . --snapshot convex-rows.json --chunk-size 100 --json # dry-run Convex nodes/edges sync plan
tsift convex-sync . --remote-snapshot --apply --endpoint https://... --json # live Convex sync transport
tsift graph-db --path . --json schema # stable provider-neutral graph DB JSON schema
tsift graph-db --path . --json refresh # materialize graph.db and report projection/tombstone operator status
tsift graph-db --path . --json status # inspect projection status without refreshing
tsift graph-db --path . --json compact # inspect post-reconciliation compaction policy
tsift graph-db --path . --json compact --apply # checkpoint WAL and VACUUM graph.db storage
tsift graph-db --path . --json backend-eval --candidate duckdb-duckpgq --candidate falkordb --candidate ladybug --candidate kuzu --candidate surrealdb --target cvxa # evaluate experimental GraphStore backend promotion gates
tsift graph-db --path . --json backend-eval | tsift metric-digest --baseline fixtures/graph-db-performance-history.json # digest graph performance history
tsift graph-db --path . --json node <id> # SQLite graph node lookup
tsift graph-db --path . --json kind backlog --property ref_id=cvxa --limit 5 # paged SQLite graph kind scan
tsift graph-db --path . --json evidence cvxa --depth 3 --limit 8 # backlog/job handoff evidence packet
tsift graph-db --path . --json evidence cvxa --depth 3 --limit 8 --cursor <next_cursor> # paginated evidence (next page)
tsift conflict-matrix --path tasks/software/tsift.md pwcm g6kf --json # parallel worker ownership/conflict report
tsift dispatch-trace --path tasks/software/tsift.md pwcm g6kf --format html # graph-backed dispatch trace
tsift dependency-dag --path tasks/software/tsift.md pwcm g6kf --json # graph-backed dependency DAG and topo batches
tsift graph-db --path . --json neighborhood <id> --depth 2 --edge-kind mentions --property path=tasks/software/tsift.md --limit 20 # bounded subgraph
tsift graph-db --path . --json path <from-id> <to-id> --max-hops 64 # bounded shortest directed path
tsift graph-db --path . --json map # two-tier overview: communities, hubs, edge kinds, modules
tsift graph-db --path . --json map --focus detect_communities # overview + focus tier for one symbol
tsift graph-db --path . --json doctor # validate local graph.db without refreshing
tsift graph-db --backend convex-snapshot --convex-snapshot rows.json --json node <id> # Convex snapshot read
tsift graph-db --backend convex-snapshot --convex-snapshot rows.json --json drift # SQLite vs Convex projection diff
tsift graph-db --backend convex-snapshot --convex-snapshot rows.json --json doctor # validate Convex rows/index metadata
tsift graph-db --path . --json related 'memory retrieval query' # first-party tsift-memory / semantic graph retrieval; use instead of direct claude-mem or /mem-search
tsift memory init . --json # initialize first-party .tsift/memory.db
tsift memory import-claude-mem . --all --apply --json # fallback migration for supported claude-mem rows into tsift-memory with per-table count reconciliation; pending_messages is reported but intentionally excluded; large reports cap event_ids and expose event_ids_total/event_ids_truncated
tsift memory status . --json # reports claude_mem_retirement=hold until full import, graph-db semantic retrieval, memory_retrieval_gate parity eval, and one normal no-direct-claude-mem session cycle are proven; rollback commands remain listed while held
tsift memory capture-agent-doc-closeout . --session-path tasks/software/tsift.md --prompt-target 'do [#id]' --response-summary '<summary>' --commit-hash <sha> --session-check-status clean --json # capture agent-doc closeout events into tsift-memory
tsift --envelope explain <symbol> --budget normal # bounded agent preview
tsift --envelope source-read src/main.rs --budget normal # AST-symbol projection with span handles and source-window expansions
tsift --envelope markdown-ast README.md --path . --budget normal # Markdown AST nodes with stable handles, hierarchy, spans, and edit/source expansions
tsift --envelope symbol-read <symbol> --file src/main.rs --budget normal # bounded symbol body packet with child refs and graph/source expansion commands
tsift edit < edits.json         # staged multi-file search/replace batch
tsift --envelope edit-intents --path . --budget normal < intents.json # validate semantic AST edit intents and emit dry-run execution plans
tsift --envelope edit-intents --path . --verify --verify-command 'cargo test' --budget normal < intents.json # verify supported intents in a temp git worktree before source mutation
tsift --envelope edit-intents --path . --apply --budget normal < intents.json # apply supported, conflict-free semantic edit intents with formatting/validation and rollback
tsift audit                     # scan installed skills, check health
tsift audit --manifest <file>   # compare against expected skill list
tsift summarize <symbol>        # cached LLM summary for a symbol
tsift summarize --extract <path>  # batch LLM extraction (one-time; relative path resolves against --path, workspace files use the matching scoped index)
tsift summarize --extract --diff  # re-extract only git-changed files within the requested path
tsift diff-digest [path]        # bounded worktree diff digest
tsift diff-digest --cached .    # bounded staged-index diff digest
tsift diff-digest --revision HEAD . # bounded single-revision/history digest
tsift impact [path]             # affected-test candidates from changed files, imports, and graph edges
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
tsift status              # auto-fixes stale indexes by default, then reports status
tsift status --no-fix     # skip auto-fix, report status only
tsift status --fix-instructions # also refresh tracked instruction files (same writes as `tsift init`)
tsift index --submodule <submod> # unknown/ambiguous workspace scopes fail closed
tsift search --strategy hybrid  # opt-in to slower hybrid BM25 + vector search
tsift search --timeout 60       # custom timeout in seconds (default: 30, 0 = no timeout)
tsift --compact search <query>  # terse human output across commands
```

`tsift session-cost` reports the cost of one transcript or runtime log. `tsift session-review` discovers the newest bounded set of matched sessions for a document or repo target and keeps two cost scopes separate: `aggregate_cost` / aggregate human fields summarize only those visible matched rows, while `latest_session_cost` reports the first/newest matched session by itself. This prevents a multi-session review from presenting cached historical spend as the active session's token total or largest turn while still preserving the bounded cross-session aggregate for trend review.

`tsift summarize --stats`, `tsift summarize <symbol>`, and `tsift summarize --file <path>` are read-only cache queries: they fail closed when `.tsift/summaries.db` is absent, never create the summary cache as a side effect, and retry against a snapshot copy when a live SQLite lock wedges the cache. In WAL mode that snapshot copy includes the sibling `-wal` / `-shm` sidecars instead of copying only the main `.db` file, so read-only fallbacks keep the same committed live state the writer was using. `--path` first resolves through the nearest ancestor `.tsift` project/workspace root, so nested directories reuse the shared summary cache instead of creating shadow caches; `summarize --file` also normalizes equivalent path spellings back to the canonical root-relative cache key, so `src/lib.rs`, `./src/lib.rs`, nested relative spellings that point at the same file, and absolute paths routed through a symlinked checkout all hit the same cached row. Summary cache rows store that root-relative key with `/` separators even on Windows, and read/delete/currentness checks also tolerate legacy `\` rows until they are rewritten. `summarize --stats` reports stale cached files when the source file is missing, when the live blake3 hash no longer matches the cached `content_hash`, and when a cached key is absolute or lexically escapes the project root (`../...`); those out-of-root cache keys count as stale/corrupt and are never opened from the filesystem. If a cached file still exists but cannot be read during stats collection, tsift counts that row as stale, completes the report, and emits a warning instead of aborting the whole command. During `--extract`, relative extract paths resolve against the caller's `--path` anchor (or that file's parent directory), then canonicalize when possible and otherwise collapse lexical `.` / `..` segments before diff filtering, stale-row pruning, and cache-key derivation, while still reusing the ancestor project's shared summary cache. tsift claims an exclusive sibling `summaries.lock` sidecar before it deletes stale rows, rechecks content hashes, or calls the LLM so concurrent extractors fail fast instead of duplicating API spend. Inside that write lock, extraction goes through a lazily-rs `SummaryCache`: each normalized file key has a Slot that reads a content-hash Cell before loading cached rows, so repeated checks for the same file/hash reuse the Slot, while a changed live `content_hash` invalidates the Slot before deciding whether to call the LLM. `SummaryCache::get_or_extract_file` owns the "current rows or compute then replace" branch so extraction skips already-current files without duplicating DB reads, and only writes replacement rows after a stale/missing Slot causes first access to compute summaries. Full re-extracts prune cached summary rows for files that no longer exist inside the requested extract scope even when that scope is now empty, workspace files resolve symbol context against the matching scoped `index.db`, symbol preload uses exact normalized file-path matches so duplicate `src/lib.rs`-style paths across scopes do not bleed into each other, symbol preload reuses the same busy-timeout plus snapshot fallback path as other read-only index consumers when a live lock is present, and `--diff` includes untracked files within the requested extract scope while deleting cached summary rows for tracked files that were removed from that scope, including the old side of `git mv` renames; on an unborn `HEAD`, `--diff` degrades to untracked-only extraction instead of failing on `git diff ... HEAD`. `tsift status` computes summary coverage against live indexed files only, so stale summary rows for deleted files do not over-report cache coverage, and it surfaces summary-cache recovery diagnostics when it had to degrade off the live database.

Summary coverage uses live indexed files that the extractor can actually process;
Markdown and other indexed-only formats are reported separately and do not
dilute the percentage. Current terminal failures are also reported separately
and removed from the automatic re-extraction recommendation.

Extraction candidates ignore one leading UTF-8 byte-order mark before testing
for Unicode whitespace. Empty, whitespace-only, BOM-only, and BOM-plus-whitespace
files are skipped before backend selection and therefore never consume a model
round trip.

`tsift edit` now stages each rewritten file beside its target and only swaps the batch into place after every edit validates and every staged file is ready. If any later swap fails, tsift restores earlier files before returning an error instead of leaving a partially-written batch behind.

Every command emitted in `status.recommendations.run`, search scale-guard
`narrow_commands`, or another machine-action field is directly executable shell
text. Stale-file counts, scope gaps, and explanatory guidance belong in their
dedicated diagnostic fields, never as parenthetical suffixes or inline comments
inside the command string.

## Search Stale Precheck + Timeout

`tsift search` now performs a cheap freshness precheck before it calls the sift engine. If an existing local index is stale, search refreshes it before spending time in the lexical engine; callers that pass `--no-autoindex` fail fast instead.

Default behavior:

- `tsift index --workspace` writes a filtered `.tsift/index.db` for files owned by the workspace root alongside the per-submodule databases. Status and federated queries report that database as `<root>`; submodule paths are excluded from it so results are neither duplicated nor allowed to bypass isolation tiers.
- fresh index: search proceeds normally
- workspace discovery recursively follows initialized or tracked gitlinks declared by nested `.gitmodules` files, including nested workspaces ignored by their parent checkout. Every discovered scope receives its own index, while ancestor indexes exclude descendant scope roots so ownership remains unambiguous.
- `graph` and `explain` accept both their compatibility positional project path and the emitted `--path <path>` form. An explicit scoped `explain` whose symbol is absent fails non-zero and names the selected scope instead of returning a successful empty report.
- stale index: search incrementally refreshes the local or scoped index before running
- missing index: search builds the local or scoped index before running when a concrete index target can be resolved

Opt-in recovery:

- `tsift search --autoindex ...` is kept as an explicit compatibility flag for the default behavior: if the local or scoped index is missing or stale, tsift incrementally builds it before searching
- `tsift search --no-autoindex ...` disables the default refresh and fails closed when an existing index is stale
- if that autoindex pass only loses the coarse `index.lock` race to another live tsift writer, search now degrades instead of failing closed: stale indexes continue with the current read-only index snapshot, missing indexes fall back to exact live-file search, and stderr includes one concise retry hint for fresh symbol/index results after the writer finishes
- `tsift search --scope <submod> --autoindex ...` rebuilds only that submodule's index
- `tsift search --federated --autoindex ...` rebuilds stale/missing federated submodule indexes before aggregating symbol hits, and its lexical/vector/hybrid sift pass only searches the same federated scope roots instead of the whole workspace
- `tsift search --scope <root> ...` explicitly selects the workspace-root index, while named submodule scopes select their own index. Unknown scopes fail closed and report every available id, including `<root>`, instead of silently searching the workspace root
- `tsift index --submodule <submod> ...` now fails closed on that same unknown or ambiguous selector set, instead of indexing `root/<submod>` into an unreachable scoped DB
- when duplicate submodules share the same trailing directory name, leaf-name selectors fail closed as ambiguous and the full `.gitmodules` path becomes the required scope id
- `tsift status`, `tsift search`, `tsift index`, `tsift locks`, `graph`, `communities`, `path`, and `explain` now resolve nested input paths against the nearest ancestor project/workspace root (`.tsift/` or workspace `.gitmodules`), so subdirectory invocations reuse the intended project/workspace indexes instead of creating nested `.tsift/index.db` state or inspecting synthetic nested lock files
- when a nested workspace path already falls under exactly one submodule source root, `tsift search`, `tsift locks`, `graph`, `communities`, `path`, and `explain` now infer that scoped index automatically instead of requiring a redundant `--scope <scope>` selector
- workspace roots that only have scoped `.tsift/indexes/<scope>/index.db` files **federate by default** for every search strategy (`#wsfed`). Previously plain `tsift search` failed closed there — but only for some queries: an identifier that auto-promoted to the `exact` backend federated fine, while anything falling through to `fts`/`lexical` exited 1 demanding `--scope <scope>` or `--federated`. Same directory, same command, same flags; the observable rule was "add `--federated` if your query has no underscore in it", which no caller can infer. Federation there is exactly what the `exact` path already did, so `--federated` is now explicit opt-in for the non-workspace case and `--scope` remains the narrowing flag. Auto-federation is decided by the same precedence the target resolver uses, so an explicit `--scope`, a path that infers a submodule or cargo package, an agent-doc task path, or an existing shared root `.tsift/index.db` all still win. A workspace whose every scope opts out of federation fails closed with that stated as the reason, rather than returning silent empty results
- `graph` and `explain` accept `--federated` and resolve the owning scope automatically at a workspace root (`#graphfed`), including the default relative `.` path and workspaces with a shared root index. Resolution walks the federated scoped indexes for a definition of the symbol, falling back to a scope that only calls it. A unique match runs within that owning scope; an exact symbol found in more than one scope fails closed, names every matching scope, and requires `--scope` because choosing the first would misrepresent one scope's call graph as a complete answer. A symbol no scope defines fails with the list of scopes searched. Federated `symbol-read` applies the same ambiguity rule and considers all exact-case matches before any case-insensitive fallback, so a higher-ranked spelling variant cannot defeat an exact match. Cross-scope call edges remain out of scope. All user-facing definition and edge coordinates in `search`, `graph`, `explain`, and `symbol-read` are one-based in human, JSON, and envelope output, matching `source-read`.
- `communities` and `path` also take `--federated` and resolve a workspace root themselves (`#wsfedrest`), so no read-only graph command still demands a scope the caller does not have. They needed different answers than `explain`/`graph`, because neither has a single symbol to resolve from. A scoped index carries only its own call edges, and that one fact settles both: there is no such thing as a cross-scope community, so `communities` runs detection **per scope** and reports each — the exact answer, not an approximation of a whole-workspace one. `--json` returns `{"scopes": [ ... ]}` with one document per scope; human output labels each with a `scope <id>:` header. And a path between symbols in two scopes does not exist to be found, so `path` resolves **both** endpoints: same scope runs there, different scopes is a precise refusal naming both. An empty result would have read as "no path in a graph containing both", which is not what happened
- `tsift search` symbol-hit reads now reopen `index.db` through the same resilient read-only helper used by other index consumers, so a live SQLite lock that appears after the stale-index precheck still falls back to a snapshot copy instead of bubbling a raw SQLite lock error
- writable index updates now claim an OS-backed exclusive lock on the sibling `index.lock` sidecar first, so concurrent `tsift index` / `tsift search --autoindex` writers fail fast with a tsift-owned error instead of surfacing raw SQLite lock contention or PID-recycling false positives
- lock diagnostics intentionally distinguish the tsift-owned `index.lock` sidecar from SQLite WAL/SHM sidecar state: if `tsift locks` sees no live `index.lock` holder but does see live WAL/SHM sidecars, it recommends checking for a wedged SQLite writer and notes that read-only status/search consumers can keep using WAL-aware snapshot fallback
- direct `tsift index`, `tsift search`, and `tsift status` regression coverage must include this WAL-without-`index.lock` mode so future changes do not regress back to raw `database is locked` failures or misleading rollback-journal-only recovery guidance
- read-only graph queries (`graph`, `communities`, `path`, `explain`) now share the same development-machine freshness chain as search: they resolve the local/scoped index target, incrementally refresh missing or stale indexes before opening `index.db`, and then read through the resilient read-only helper; if a concurrent tsift writer already holds `index.lock` and a prior database exists, they skip the refresh with a concise stderr note and continue against the current read-only snapshot
- `source-read` symbol refs and `symbol-read` join that same freshness chain: they resolve the per-file/per-scope index target (including the per-cargo-package `.tsift/indexes/cargo/<package>/index.db` for a workspace member) and build or refresh it on demand before reading. Previously these read paths checked only whether the resolved index already existed, so a workspace member that no prior graph/search/explain query had indexed silently degraded — `source-read` dropped to window-only output with an `index refs unavailable: no index found at …` warning and `symbol-read` failed closed — even though `tsift status` reported the shared root index fresh. They now build the missing per-package index on first read, so AST symbol projection is available for any member, and only fall back (with the warning / a fail-closed error) when the build itself cannot complete (for example a concurrent writer holds the lock with no prior snapshot)
- agent-doc task paths named after workspace scopes, such as `tasks/software/tsift.md`, resolve to that scope's index for both search and read-only graph queries, so graph-backed agent workflows do not require an extra `--scope tsift` flag
- writable `index.db` opens also set `PRAGMA wal_autocheckpoint=256`, so normal tsift write traffic checkpoints the WAL on an explicit budget instead of leaving it entirely to SQLite defaults
- non-fatal source-read / symbol-extraction / call-extraction failures now emit warnings instead of being silently swallowed, and those warnings are carried in `IndexSummary` for JSON consumers

`tsift search` still wraps the sift engine call in a 30-second timeout (configurable via `--timeout`). Timed searches now run in an internal helper process so a timeout kills the underlying sift work instead of leaving a detached worker thread behind. The timeout remains a backstop for genuinely slow lexical searches or for sessions that reach search without a usable index.

Both the in-process lexical path and the timed `__search-worker` helper now point sift at a stable `.tsift/search-cache` directory under the resolved project/workspace root. That keeps corpus/BM25 artifacts reusable across repeated searches, including scoped and federated queries that execute from subpaths but still belong to the same root-owned `.tsift/` state.

`tsift search --exact` (and `--strategy exact`) bypasses that lexical/index precheck entirely and executes a literal `rg -F` scan instead. Plain `tsift search <query>` also auto-promotes single-token identifier/path-like queries such as `claudescore-3`, `alpha_helper`, `src/main.rs`, and `crate::module` to that exact backend by default. That path keeps rg-style lookups fast, works even when the symbol index is stale or missing, and does not require a shared root `.tsift/index.db` in workspaces that only maintain scoped indexes.

When an exact or otherwise high-hit search returns repeated matches from the same file, the default human output now collapses those repeated line hits into one file-level entry with hit counts first and only a couple of representative snippets after that. That keeps broad literal lookups usable without relying on external line truncation alone.

A `--path <subdir>` that narrows to a strict subdirectory of the resolved project/workspace root now sub-narrows the FTS/lexical result set to that subdirectory (#015t Phase 4b enhancement, `#ve5f`). The FTS5 `content_fts` path searches the whole project index regardless of `--path` (its stored paths are absolute), so before this change a sub-path argument scoped only the *symbol* prepass, never the lexical hits — unlike `--exact`, where `rg` already runs inside the sub-path. The non-exact result set is now pruned to hits whose path resolves under that sub-scope, giving `--path` the same narrowing effect across exact and lexical search. Each surviving hit keeps its original BM25 `rank` (the result set stays a strict subsequence of the global ranking, so narrowing changes which files appear, never their relative order). The prune is a no-op for `--exact` (already scoped) and is skipped for `--federated` search, where a single sub-path must not drop cross-repo hits; a `--path` at (or above) the project root preserves the whole-index default. `--path`/`-p` is **repeatable**: multiple `--path` arguments are searched together, so `tsift search --exact <q> --path a --path b` forwards both paths to ripgrep, and the lexical result set is pruned to the **union** of the provided sub-scopes (a hit is kept if it resolves under any of them). Single-path and no-path behavior is unchanged.

The default lexical/non-exact path is the native FTS5 `content_fts` table inside `.tsift/index.db` (#015t Phase 4 cutover), returning `strategy: "fts"` with BM25 file ranking; exact-identifier lookups stay on ripgrep. The in-memory `TokenIndex` (a tokenized OR-union inversion built by walking the live tree) is retained only as a **degraded-read-only fallback** — reached when the root `index.db` is missing/corrupt, or stale because a concurrent writer holds it (autoindex degraded to read-only), or by direct programmatic callers. That fallback is always **live** (it rebuilds from source at query time; the never-invalidated `token-index.json` cache was deleted in Phase 4b). #015t Phase 4b(a) decision (operator, 2026-06-20): keep this in-memory rebuild rather than replacing it with a literal `rg -F` walk. Both are equally live, so the choice is about parity, not freshness — the rebuild preserves the same OR-union matching and ranking as the FTS path, so a transient degraded window behaves indistinguishably from the healthy path, whereas `rg -F` would silently switch the fallback to literal substring matching with no ranking.

`tsift explain` keeps the full JSON/tabular edge list, but the default human-readable caller/callee sections now collapse dense same-file edge sets into grouped file rows with counts. That reduces token volume for highly-connected symbols while preserving the concrete caller/callee names in the grouped summary.

## Bounded Source-File Reads

`tsift source-read <file>` defaults to an AST-symbol projection for source inspection. It is intended for agent workflows that would otherwise re-read whole files after search results or diagnostics. Relative file arguments resolve inside the nearest project/workspace root discovered from `--path`, and paths outside that root fail closed. Pass `--style window` when a literal numbered source preview is needed.

The default JSON and envelope forms (`tsift --envelope source-read src/main.rs --budget normal`) emit:

- a stable `sast-*` AST projection handle for the file/range
- `ssym-*` symbol refs for indexed symbols intersecting the selected range, each with a `symbol-read` expansion command and optional AST `span` metadata (`span-*` handle, node kind, byte range, body range, parent handle, and child handles). Markdown refs include full heading section ranges, `markdown.heading_level`, `markdown.section_path`, `markdown.section_handle`, `markdown.list_depth`, and `markdown.fence_language` where applicable.
- cached `sum-*` summary refs for the file when `.tsift/summaries.db` is present, each with a `summarize` expansion command
- Markdown files include a bounded `markdown` projection with an outline-first section/block preview, stable `mdast-*`/`span-*` handles for visible nodes, and selected-node expansion commands
- explicit `expand.window`, `expand.file_window`, and Markdown AST expansion commands so the next read can expand incrementally into source lines only when a literal preview is needed

`--start`, `--lines`, and `--end` bound the AST projection range without changing the default AST output. `--style window` restores the legacy source preview mode: it emits a stable `swin-*` window handle, line-numbered preview rows capped by the response budget and body token cap, intersecting `ssym-*` refs, summaries, Markdown projection data, and explicit `before`/`after`/`body`/full-file expansion commands. The command still returns a structured packet when index or summary stores are missing; those enrichment failures are reported as warnings. `--scope` restricts index refs for workspace submodule indexes, and nested paths infer the matching workspace scope when possible.

Budget truncation preserves source-leading spaces and tabs byte-for-byte and removes only trailing whitespace, so a read-to-edit workflow never loses indentation. This applies to shared source projections from `source-read`, `symbol-read`, `search`, `context-pack`, and `session-review`.

`tsift markdown-ast <file>` is the first-class Markdown projection surface. It parses the current `.md`/`.mdx` buffer directly with tree-sitter Markdown, reuses an in-process per-file/content-hash parse cache across Markdown edit planning/apply and source-read enrichment, and emits bounded block-level nodes for headings, list items, and fenced code blocks. The shared `tsift-md-ast` leaf crate also exposes `MdTextEdit`, `reparse_incremental()`, and `reparse_incremental_with_input_edit()` so CRDT/live-document consumers can update one Markdown source-range edit against a previous tree without depending on tsift's graph/index stack. Supported fenced-code languages (`rust`, `python`, `typescript`, `javascript`, `tsx`, `jsx`, `kotlin`, `zig`, `bash`, `gdscript`, plus common extension aliases) are parsed as embedded language islands; code-block node metadata includes `embedded_symbols[]` with stable `span-*` handles, language/kind/node kind, absolute Markdown-file byte spans, and line ranges for the extracted symbols. Reports include `projection.mode` (`outline_first` or `selected_node`), `projection.cache` with source hash, cache-hit flag, node counts, and parse duration, plus phase timings for parse/extract and outline projection. Each node carries:

- a stable `mdast-*` node handle plus the corresponding `span-*` byte-span handle used by source-read, symbol-read, and edit-intents target metadata
- 1-based line range, byte span, optional body byte span, parent handle, child handles, and heading-derived `section_path`
- block metadata: `block_kind`, heading level, section handle, list depth/marker/order, and code-fence language/marker
- expansion commands for the node source window, body window, `symbol-read`, and `edit-intents`

The default projection is outline-first: headings are surfaced before bounded list/code block previews so session-review and context-pack can carry a compact document map before selecting bodies. `--node <mdast-*|span-*>` switches to selected-node mode and focuses the projection on one known node handle. This lets `symbol-read` hand a Markdown target span directly to `markdown-ast --node` without requiring consumers to re-scan the whole document, while edit-intents dry-run plans keep using the same stable `span-*` handle for conflict-aware write planning.

`tsift symbol-read <symbol>` is the symbol-centered read replacement surface. It resolves the query through the indexed symbols table, optionally scoped by `--file` and `--scope`, then emits:

At a workspace root, `symbol-read` federates over `<root>` and eligible submodule indexes by default; `--federated` makes that choice explicit. An exact symbol owned by more than one scope fails closed with the ambiguous scope ids and asks for `--file` or `--scope`. A root-owned `--file` resolves against the filtered `<root>` index.

- a stable `sread-*` handle for the selected symbol
- the symbol signature/range metadata, optional AST `span` metadata, and a token-budgeted body preview capped by both a line-count budget (`preview_items × 16` lines) and a body token cap (default 1500 tokens for Normal, 500 for Small, 3000 for Deep); when the body exceeds the token cap it is truncated and an `expand.body` command is emitted for the remaining body lines
- child `ssym-*` refs discovered inside the selected symbol's AST byte span when available, falling back to indexed lines for older indexes
- cached summary refs for the owning file when available
- expansion commands for the selected source window, remaining body, whole file, `explain`, caller graph, callee graph, and `markdown-ast --node` when the selected symbol is Markdown

`source-read` symbol refs now expand to `symbol-read`, while `symbol-read` preserves `explain`, graph, and Markdown AST commands as secondary expansion links. This makes whole-file `Read` fallback unnecessary for the normal search -> source window -> symbol body/navigation path.

`tsift edit-intents` is the semantic write-planning and guarded write-executor surface. It accepts JSON `{ "intents": [...] }` batches with normalized code intent kinds `rename_symbol`, `replace_function_body`, `insert_import`, `add_method`, `update_call_signature`, `move_declaration`, and `rewrite_call_sites`, plus Markdown intent kinds `rename_heading`, `replace_section_body`, `insert_section`, `move_section`, `insert_list_item`, and `rewrite_code_fence`. The command resolves symbol/file targets against the current index, reports the current content hash, target line range, and target symbol `span` when the index has AST spans, detects optional `expected_content_hash` conflicts, and emits dry-run plans with bounded diff previews by default. For call-site intents, plans include same-file indexed `call_refs`; Rust rewrites currently fail closed when indexed refs cross the target file. Markdown heading targets resolve to full section spans, list/code-fence targets carry stable byte/body ranges, and Markdown span metadata carries hierarchy, section path, list depth, and fence language. Markdown section intents `rename_heading`, `replace_section_body`, `insert_section`, and `move_section` are apply-capable: the executor re-parses current Markdown buffers for each intent, validates output with tree-sitter Markdown, supports `destination_symbol` plus `position=before|after` for section moves, and writes through the same atomic edit/rollback path as code intents. Markdown block intents `insert_list_item` and `rewrite_code_fence` are also apply-capable: `insert_list_item` requires a unique list-item target, preserves marker and indentation, and supports `position=before|after`; `rewrite_code_fence` requires a unique code-fence target, replaces only the fence body, refuses replacement text with fence markers, and preserves the existing fence syntax. With `--verify`, Markdown section/block intents use the same detached temp-worktree gate as code intents: temp apply, reindex, source-read windows, impact summaries, optional `--verify-command`, and fail-closed no-mutation behavior before any real `--apply`.

With `--verify`, supported intents are applied first in a detached temporary git worktree, that worktree is reindexed before and after the temp apply, bounded `source-read` windows and an `impact` summary are run against the temp result, and an optional `--verify-command '<shell command>'` must pass before the real tree can be mutated. With `--apply`, supported Rust intents (`rename_symbol`, `replace_function_body`, `insert_import`, `add_method`, `rewrite_call_sites`, `update_call_signature`, `move_declaration`), script-language intents for TypeScript/TSX, JavaScript/JSX, and Python (`rename_symbol`, `replace_function_body`, `insert_import`), `rename_symbol` for the remaining indexed languages (Kotlin, Bash, Zig, GDScript), and Markdown section/block intents are composed per file and committed through the same backup/rollback atomic edit path as `tsift edit`. Rust output is formatted through `rustfmt --edition 2024` before any source file is swapped. TypeScript/JavaScript output is tree-sitter validated and formatted with a local `prettier` when available; Python output is tree-sitter validated and formatted with local `ruff format` or `black --quiet` when available. Markdown output is tree-sitter validated and has no formatter. `rewrite_call_sites` replaces indexed Rust call expressions with `replacement`; `update_call_signature` replaces the function signature with `replacement` and requires `call_replacement` when same-file call refs must be rewritten. `add_method` targets an indexed Rust `struct` or `enum`, inserts the method into an existing inherent `impl`, or creates a new inherent `impl` next to the type. `move_declaration` moves an indexed Rust declaration into an existing same-directory destination file named by `file`, reports `destination_file`, inserts the declaration after the destination prelude, and normalizes the source module with `mod <destination>;` plus `use <destination>::<symbol>;`. Markdown `move_section` moves a heading section before or after another heading in the same file using `destination_symbol` and optional `position`; `insert_section` appends by default or inserts before/after a target heading when `symbol` and `position` are supplied. Markdown `insert_list_item` inserts before or after a unique list item target using the target's marker and indentation; `rewrite_code_fence` rewrites the body of a unique fenced code block while preserving its fences. Conflicts, unsupported intent kinds, unsupported languages, ambiguous Markdown block targets, cross-file call refs, missing call replacements, unsupported structural destinations, AST validation mismatches, temp-worktree verification failures, failing verification commands, and formatter failures fail closed before mutation.

Additional semantic edit language support is contract-backed. A language executor must declare one `SemanticEditLanguageContract` entry with a canonical language id, display name, `graph::Lang` parser binding, formatter staging suffix, language aliases, file extensions, recognized intent kinds, apply-supported intent kinds, language-family behavior, and formatter policy. The contract is the source of truth for language/file resolution, dry-run support flags, apply refusal messages, temp-worktree verification, parser validation, and formatter selection. New languages must also add executor coverage for every apply-supported intent, no-mutation refusal coverage for unsupported recognized intents, and a contract test that proves aliases, extensions, formatter policy, recognized intents, and apply-supported intents are complete before the language is documented as apply-supported.

## Handle-Preserving Search Workflow

`tsift workflow search` prints a composable recipe for agents that need to move from literal lookup to broader retrieval without losing stable handles. The JSON and envelope forms (`tsift --envelope workflow search`, `tsift workflow search --json`) list ordered steps for:

- exact anchors: `tsift --envelope search "<literal>" --exact --path . --budget normal`
- semantic broadening: `tsift --envelope search "<concept>" --strategy hybrid --path . --budget normal`
- graph expansion: `tsift --envelope explain "<symbol>" --path . --budget normal`
- summary reads: `tsift summarize "<symbol>" --path . --json`
- digest expansion: `tsift --envelope context-pack <path> --test-input test.log --log-input build.log --budget normal`

The contract is to keep every emitted handle with its originating command, query, path, and strategy, then use each result's `expand`, `follow_up`, or `resume_commands` field for the next command while citing the parent handle. Search previews preserve `sfam-*` and `shit-*` handles, explain previews preserve `edef-*`, `ecall-*`, and `eces-*` handles, and digest/context-pack outputs preserve artifact and touched-symbol handles across diff, test, log, and session expansions.

`tsift workflow kg` (aliases `knowledge-graph`, `kg-workflow`) prints the companion recipe for the local Knowledge Graph: smoke-check the Ollama extractor, `kg extract --input <file> --source-ref <file>`, `kg status`, `kg refresh` (with `--apply` to re-extract drifted sources), and `kg evidence --symbol "<symbol>"` as the agent-doc read seam over `.tsift/graph.db`. The contract is to extract once per source and answer reads with `kg status`/`kg evidence` rather than re-extracting; `kg evidence` takes `--symbol`/`--kind` (not a positional) and has no `--budget` flag. `tsift workflow <unknown>` fails closed listing the available recipes (`search, kg`).

To make the Knowledge Graph discoverable, `tsift status` promotes `kg` into its `use:` recommendation list (after `graph`) whenever a project `.tsift/graph.db` is present; with no graph.db the `use:` list omits `kg` so agents extract before they read.

When an index is present, the AST symbol-ranking prepass is now bounded: SQLite only pulls exact-name rows and overlapping-tag candidates, orders them by exact/tag overlap, and caps that candidate scan to the requested search `--limit` instead of loading the full `symbols` table into memory first.

### Symbol-match ranking (`#symnoise`)

Symbol matches are printed first and carry the highest-confidence framing, so a
caller that reads that section and stops — the whole point of a bounded envelope
— must not be handed wrong locations. Three rules keep the section honest:

- **Query coverage floor.** A partial-tag hit must cover at least half the
  query's tags, rounded up. `_run` shares exactly one of `run_scale_helper`'s
  three tags; admitting it spent the caller's whole symbol budget on unrelated
  short functions in an unrelated scope, ranked above the one file that actually
  contained the identifier. The floor is enforced in SQL as well as in Rust, so
  the candidate `LIMIT` is spent on rows that survive it.
- **Precision-weighted scoring.** Partial hits score by F0.5 rather than F1,
  weighting coverage of the *query* over recall against the symbol. Plain F1
  scored every one-of-three match at a flat `0.5000`, because a single-tag symbol
  has perfect recall — so "three matches at the same score" read as genuine
  ambiguity rather than as noise, with no signal to discriminate on. Under F0.5 a
  one-of-three match lands near `0.38` and a two-of-three near `0.71`.
- **Keyword-only queries are lexical.** A query whose every tag is a language
keyword (`def `, `func`, `class fn`) cannot identify a symbol by name — every
Python file contains `def `, so tag matching just surfaced whichever symbols
happened to end in `_def`. Tag matching is skipped for those queries and the
lexical strategy answers them. Exact-name matching still applies, so a symbol
literally named `def` is still found.
- **General search symbols are code-only.** Markdown headings, list items, and
other document-structure nodes remain indexed for `symbol-read`, Markdown AST
navigation, and semantic editing, but the ordinary and federated `search`
symbol sections exclude them. Matching prose still appears through the lexical
result section instead of masquerading as a code symbol.

When nothing clears the floor the symbol section is empty rather than padded to
`--limit`: an empty symbol section plus a real lexical hit is a better answer
than three confident wrong ones.

## Index Quiet Mode

`tsift index --quiet` (or `-q`) suppresses the per-file change list, printing only the summary line. `--exit-code` implies `--quiet`.

Without `--quiet`, `tsift index --check` on a large repo with 14K+ stale files outputs every file path (1.7MB / 433K tokens in human mode, 2.6MB in JSON). With `--quiet`, output is a single summary line (~80 bytes human, ~120 bytes JSON).

In JSON mode, `--quiet` also omits the `changes` array and uses compact (non-pretty) serialization.

## Init (Project Setup)

`tsift init` ensures the Code Navigation section is present in `AGENTS.md` for Codex-style harnesses and mirrors it into `CLAUDE.md` when that file exists, so local agent sessions prefer envelope previews plus artifact-backed digest surfaces over raw file reads, diffs, and verbose logs.

```bash
tsift init                              # ensure AGENTS.md (and CLAUDE.md if present) in current directory
tsift init <path>                       # inject at <path> (dir or file)
tsift init src/sub/tasks/plan.md        # resolves to submodule root src/sub/
tsift init --codex                      # also inject auto-reindex hook into .codex/hooks.json
tsift init --codex --workspace          # resolve to workspace root + install one workspace hook
tsift init --opencode                   # install .opencode/commands/tsift-*.md shortcuts
opencode plugin opencode-tsift          # install the same shortcuts after the npm package is published
```

### Path Resolution

`tsift init` resolves the target directory before operating:

1. If `<path>` is a file, use its parent directory
2. Run `git rev-parse --show-toplevel` from that directory to find the git root (handles submodules)
3. Fall back to the directory itself if not in a git repo

This means `tsift init src/session-share/tasks/claudescore-3.md` resolves to `src/session-share/` — the submodule root — and initializes there. When the resolved path differs from the input, a `resolved: <input> → <target>` line is printed.

`--workspace` keeps the same resolved project root. In particular, invoking it
inside a git submodule initializes that submodule's workspace rather than
promoting the target to the outer superproject.

### Behavior

1. Ensures `.tsift/` is ignored. Before changing `.gitignore`, it asks Git for the effective ignore decision, so `.git/info/exclude`, a global excludes file, a parent rule, or a broader tracked pattern remains authoritative. When another source already ignores the path, `init` leaves `.gitignore` untouched and reports that source.
2. Ensures `AGENTS.md` exists with the section (creates it if needed)
3. Writes `.agent/runbooks/code-navigation.md` with the full command detail the section defers to, under its own `<!-- tsift:code-navigation-runbook -->` markers (creates the directory and file if needed, updates the marked region in place, preserves text outside the markers). If the canonical path is absent but the legacy `runbooks/code-navigation.md` exists, init moves that file first so hand-written text outside the managed markers survives the migration.
4. If `CLAUDE.md` exists **and does not already defer to `AGENTS.md`**, updates or appends the same section there too
5. If `CLAUDE.md` defers to `AGENTS.md` — it resolves to the same file (symlink), or it pulls it in with a Claude Code `@AGENTS.md` import — no section is injected. An already-present managed section in an `@AGENTS.md`-importing `CLAUDE.md` is **removed**, since that file already inherits the canonical copy. A `CLAUDE.md` symlinked to `AGENTS.md` is left untouched: rewriting through the link would strip the section out of the canonical file
6. If the section already exists (detected by `<!-- tsift:code-navigation -->` markers), updates it in place
7. Idempotent — running twice produces no changes on the second run
8. With `--codex`: merges a `UserPromptSubmit` auto-reindex hook into `.codex/hooks.json` (creates the file and directory if needed, updates stale tsift commands in place, removes duplicate tsift hook entries, idempotent)
9. With `--opencode`: installs marker-owned `.opencode/commands/tsift-*.md` command templates for status, session review, context pack, diff digest, test digest, log digest, rewrite-run, explain, symbol-read, and graph workflows. Existing marker-owned files are updated idempotently; unmanaged same-name command files fail closed instead of being overwritten. The same marker-owned templates ship in the publishable npm `opencode-tsift` package; after it is published, installing it with `opencode plugin opencode-tsift` gives OpenCode users a registry install path that does not require cloning the tsift repository.
10. When the resolved target has `.gitmodules`, the Codex hook automatically uses `tsift index --check --exit-code --workspace <root>` / `tsift index --workspace <root>` so one root hook covers initialized submodules. `--workspace` makes that root resolution explicit from inside a submodule.
11. The injected Code Navigation section explicitly tells harnesses to switch to the owning repo or submodule root before running tsift/build/test commands, so submodule work does not inherit the wider superproject instruction surface by accident.
12. The injected section also steers harnesses toward envelope-backed `search`, `explain`, `session-review`, `context-pack`, and digest-runner artifacts instead of raw transcript replays, `git diff/show/log` patch dumps, or verbose build/test output reads.
13. Verification guidance is capability-based. A Makefile contributes `make check` only when it defines a `check` target; otherwise a `justfile` contributes the first unambiguous `check`, `test`, or `verify` recipe. GitHub Actions contributes `gh run list --limit 1` only when `.github/workflows/` exists and `gh` is executable; GitLab contributes `glab ci status` only when `.gitlab-ci.yml` exists and `glab` is executable. Missing or ambiguous capabilities emit no verification sentence instead of a command that will fail.
14. The injected section is a hot-path router, not a manual: it carries the session-start rule, the envelope-over-raw-read substitutions, and any detected verification rule, and defers budgets, `tsift workflow search`, `report.scale_guard` handling, the `tsift rewrite --run` path for harnesses without `PreToolUse` hooks, and Codex/OpenCode integration to `.agent/runbooks/code-navigation.md`. Because `tsift init` generates that runbook itself, the pair ships together in every initialized checkout — a standalone checkout is never left with a pointer to a file that does not exist. A repository that also ships a current `.claude/skills/tsift/SKILL.md` should use that skill as the deeper source.

15. With `--workspace`, the instruction surface is refreshed in **every** workspace scope, not only the superproject (`#wsinit`). `status` already maintains index state per scope, so stopping instructions at the root produced a workspace whose index was uniformly current and whose instruction blocks were not — and the stale blocks are not merely old, they teach flags this release deprecated and a runbook path 0.1.81 migrated away from. Each scope prints a `scope <id>: <path>` header followed by the same per-path lines the root emits. Harness integrations (`--codex`, `--opencode`) stay at the root the operator invoked them from; only the Code Navigation block and its runbook fan out, because that is what `status` reports on and what a submodule-local harness actually loads. A scope opts out with `instructions = false` under its `.tsift/config.toml` override and prints `scope <id>: skipped (instructions = false in .tsift/config.toml)`.

The OpenCode command shortcut set is intentionally prompt-template based rather than a background hook: OpenCode already reads project `AGENTS.md`, and the managed commands give operators explicit `/tsift-status`, `/tsift-session-review`, `/tsift-context-pack`, `/tsift-diff-digest`, `/tsift-test-digest`, `/tsift-log-digest`, `/tsift-rewrite-run`, `/tsift-explain`, `/tsift-symbol-read`, and `/tsift-graph` entrypoints that route common workflows through bounded tsift evidence without depending on raw terminal replay.

`packages/opencode-tsift` is the npm distribution for those same templates. The plugin writes the marker-owned command files into the active project's `.opencode/commands/` directory on load and refuses unmanaged same-name conflicts, matching `tsift init --opencode`. Its `opencode-tsift` CLI entrypoint can also refresh the files directly. The package version follows `Cargo.toml`, and release verification checks that the packaged command files exactly match the Rust `tsift init --opencode` output before an npm publish can run.

On plugin load and on the `installation.updated` lifecycle hook, the plugin runs an automatic freshness check: it calls `tsift status --json` to read the index state and, if the index is stale or missing, runs `tsift status` to reindex. It never passes an instruction-rewriting flag, so plugin load cannot dirty a tracked file. This mirrors the Codex `UserPromptSubmit` auto-reindex hook but triggers at plugin load time since OpenCode does not expose a prompt-time hook system. Reindex errors are logged but never block plugin load.

### Injected Section

```markdown
<!-- tsift:code-navigation v=0.1.81 -->
## Code Navigation

Run `tsift status` at session start from the owning repo root. If the task or file lives under a git submodule (for example `src/tsift/...`), switch to that submodule root first so the harness loads the narrower local instructions and repo state instead of the superproject root. `tsift status` repairs the `.tsift/` index state it owns and never rewrites tracked files (`--no-fix` skips even that). If status reports stale or missing instructions, run `tsift init` to refresh the tracked Code Navigation block and runbook; it names every tracked file it rewrites or moves. When the harness cannot perform write commands, ask the user to run the printed `run:` command instead.

Prefer tsift envelopes over raw reads:
- `tsift --envelope search <query>` instead of `grep`/`rg`
- `tsift --envelope source-read <file>` / `tsift --envelope symbol-read <symbol>` instead of `cat`/`head`
- `tsift --envelope explain <symbol>` and `tsift graph <symbol> --callers` / `--callees` for call graphs
- `tsift diff-digest [path]` instead of `git diff`, `git show`, or patch-style `git log`
- `tsift --envelope session-review <path>` / `tsift --envelope context-pack <path>` instead of replaying long session docs, transcripts, or runtime logs
- `tsift --envelope digest-runner --kind test|log --path . --shell-command '<command>'` instead of raw test/build output

Command detail lives in [`.agent/runbooks/code-navigation.md`](.agent/runbooks/code-navigation.md) — budgets, `tsift workflow search`, `report.scale_guard` handling, the harness rewrite path for `PreToolUse`-less harnesses, and Codex/OpenCode integration. `tsift init` writes and versions that runbook alongside this block, so it is present in every initialized checkout; read it before broad exploration instead of expanding this block. A repository that also ships a current `.claude/skills/tsift/SKILL.md` should use that skill as the deeper source.

When detected, this position carries repository-valid local and CI verification commands. It is omitted when no supported command can be proven from the repository and current host.

Only read full source files when tsift results are insufficient.
<!-- /tsift:code-navigation -->
```

### Generated Runbook

`.agent/runbooks/code-navigation.md` holds the detail the block defers to, under its own marker pair so the two surfaces version independently of any hand-written text around them:

```markdown
<!-- tsift:code-navigation-runbook v=0.1.81 -->
# Code Navigation

Managed by `tsift init` (versioned markers) — do not hand-edit between the markers; re-run `tsift init` to refresh. Text outside the markers is preserved.

## Session start
## Search, read, and graph
## Bounded digests instead of raw output
## Harnesses without `PreToolUse` hooks
## Verification (present only when a supported command is detected)
<!-- /tsift:code-navigation-runbook -->
```

The runbook marker name extends the block's, so the block's marker prefix is matched including its trailing space (`<!-- tsift:code-navigation `); otherwise the block logic would claim `<!-- tsift:code-navigation-runbook ... -->` as its own opening marker.

The `AGENTS.md` block and the runbook are one instruction surface with two files. `tsift status` reports `instructions: stale` when either the block marker version or the runbook marker version differs from the installed tsift, or when the runbook is missing entirely — so a repository initialized before the split is refreshed by the same `tsift init` / `tsift status --fix-instructions` it already recommends.

The HTML comment markers enable idempotent updates without parsing markdown structure.

### Version Markers

The opening marker embeds the tsift version (`v=X.Y.Z`) that generated it. When tsift is upgraded:

- `tsift status` reports `instructions: stale` and recommends `tsift init`
- `tsift init` detects the older version marker and replaces the section with the current version's content
- Pre-versioned markers (no `v=` attribute) are treated as stale
- The generated runbook carries its own `<!-- tsift:code-navigation-runbook v=X.Y.Z -->` marker and is checked the same way; a current block marker with a stale or missing runbook still reports `instructions: stale`, because the block delegates to a file that must exist and match

This ensures agent sessions always use instructions matching the installed binary.
Release-bump regressions are covered through the compiled CLI path: a stale Code Navigation marker from the previous binary version must be rewritten by `tsift status --json`, and the final JSON report must show `instructions.state=current` for the installed version.

## Status (Session Health Check)

 `tsift status` reports index freshness, instruction version, summary cache availability, and a machine-parseable `use:` list so the agent knows which tsift commands are worth calling this session. When the input path is a nested subdirectory, `status` first promotes it to the nearest ancestor that already owns `.tsift/` so the check reuses the existing project/workspace state, but it stops at a nested git root before considering parent `.tsift/` directories and ignores ambient system-temp-root project markers for child temp dirs so unrelated temp or parent workspaces cannot capture a child repo. On workspace roots, it treats scoped indexes under `.tsift/indexes/<scope>/index.db` as the authoritative status surface even if a shared `.tsift/index.db` also exists. If one or more configured workspace scopes are present on disk but their scoped `index.db` files are missing, the CLI auto-builds just those missing scoped indexes before it prints the final status so a partially initialized workspace does not stay stuck at `index: missing` / `stale` after a successful status pass. `tsift status` automatically applies safe local fixes to the state it owns when the index is stale: refresh stale or missing indexes, rebuild all existing workspace scopes when the workspace index is stale, evict expired cycle-packet cache entries, and then print the final status. Every one of those writes lands under gitignored `.tsift/`. Tracked-file writes are a separate class and are never performed by a bare `status`: refreshing the Code Navigation block, the managed runbook, and any legacy-runbook relocation requires the explicit `tsift init` or `tsift status --fix-instructions`, because a read-shaped command must not leave an unrequested diff in a version-controlled tree. When instructions are stale, bare `status` reports `instructions: stale (... — run tsift init)` and folds `tsift init` into the `run:` recommendation instead of writing. An instruction refresh names each tracked path it touches on stderr, one line per path (`status fix: rewrote AGENTS.md (v0.1.81 -> v0.1.82)`, `status fix: moved runbooks/code-navigation.md -> .agent/runbooks/code-navigation.md`), and `tsift init` prints the same relocation line, so a tracked move is never a silent deletion. Within one status command, lazily-rs backs a per-cycle inspection cache so repeated status passes reuse index inspection results and summary coverage can reuse tracked file paths without reopening the same SQLite index; any status-triggered index mutation invalidates the cache before the next pass. Use `--no-fix` to skip auto-fix and report raw status; `--no-fix` also suppresses `--fix-instructions`. The deprecated `--fix` flag still works and now means exactly `--fix-instructions`, preserving the pre-0.1.81 behavior that made tracked-file rewrites an explicit opt-in.

Generated instruction surfaces must never teach a flag tsift has deprecated. `init::DEPRECATED_FLAG_USAGES` lists the deprecated forms, and a unit test asserts the `AGENTS.md` block, the managed runbook, and every generated OpenCode command body are free of them — so a release that deprecates a flag cannot ship templates that still recommend it. When status recommends `tsift summarize --extract ...`, that extract scope is derived from the indexed layout: it uses the common indexed root (for example `src/` when every tracked file or scope lives under `src/`) and falls back to `.` when the indexed files span the project root or multiple unrelated workspace roots.

When the index is stale, `status` also emits a lightweight `reminders` list in JSON and a matching human `reminders:` section. The reminder repeats the concrete reindex command, includes the stale-file or missing-scope count, and notes when no summary cache is available so agents know to refresh the index before relying on search/explain/graph and to run `tsift summarize --extract <scope>` after the index is fresh when summary refs are needed.

```bash
tsift status            # auto-fixes stale index/instructions by default, human-readable output
tsift status --json     # structured JSON output (also auto-fixes by default)
tsift status <path>     # check a specific codebase directory
tsift status --no-fix   # skip auto-fix, report raw status
tsift status --fix-instructions # also rewrite tracked instruction files
```

For a multi-scope workspace, automatic repair is lazy by scope: missing scopes
are initialized and stale scopes are refreshed, while already-fresh scopes are
not rescanned merely because another scope needs repair.

### Output

Four sections: index state, instruction version, summary cache state, recommendations.

When everything is available:
```
index: fresh (last indexed 2m ago, 200 files tracked)
instructions: current (v0.1.0)
summaries: 142/200 extraction candidates cached (71%), 8 indexed files not extractable
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

### Language coverage (`#goindex`)

`status` reports the scopes where the index walk dropped a meaningful share of
the files it saw because no indexer language claims their extension. Without it,
a scope indexing 8 of its 26 tracked files still printed `fresh (… 8 files
tracked)`, and the gap surfaced only as confident empty search results — the
worst shape for an agent that AGENTS.md tells to prefer `tsift search` over
`grep` and to read full source only when tsift is insufficient.

```
language coverage:
  scope go-tool: indexed 8 of 26 walked files — skipped .go 7, .json 6, .txt 5
```

A gap is reported only when the skipped files are at least a quarter of the walk
*and* the dominant skipped extension costs three files or more, so a repo with a
stray `.txt` beside 600 indexed sources does not grow a warning. The filtered
workspace-root scope is the narrow exception: a skipped file that is at least a
quarter of that usually-small root walk is reported even when it is the only file
of its extension, and is labeled `<root>`. `tsift index`
reports the same fact per run as `skipped: N (unsupported extension) — .go 7,
.json 6`; `--compact` shortens it to `unsupported:N`. Workspace reports apply
the same coverage check to the filtered workspace-root index and label that
owner as `<root>`, as well as checking each configured submodule scope.

### Per-scope instruction state (`#wsinit`)

Index freshness is reported per scope; instruction state used to be a single
workspace-level line, so submodules left behind by `init --workspace` were
invisible from the workspace root. That is the drift that matters most: the
Code Navigation block tells an agent to switch to the submodule root so the
narrower local instructions load, which makes the un-refreshed file the one
actually consulted.

```
instructions: current (v0.1.83)
instructions: stale in 3 of 6 scopes (run tsift init --workspace)
  scope py-api: stale (v0.1.80)
  scope go-tool: stale (v0.1.80)
  scope viewer: missing
```

Scope drift also reaches the `run:` recommendation, so a workspace whose
superproject block is current but whose submodules are not still recommends
`tsift init --workspace`. A scope opts out with `instructions = false` under its
`.tsift/config.toml` override; an opted-out scope is omitted from this list
rather than reported as `missing`.

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
  "scope_instructions": [
    { "scope": "py-api", "instructions": { "state": "stale", "found": "0.1.80", "expected": "0.1.83" } }
  ],
  "language_coverage": [
    {
      "scope": "go-tool",
      "indexed_files": 8,
      "skipped_files": 18,
      "dominant_extension": ".go",
      "dominant_extension_files": 7,
      "skipped_by_extension": [[".go", 7], [".json", 6], [".txt", 5]]
    }
  ],
  "summaries": { "state": "available|none|unavailable", "cached_files": N, "total_indexed_files": N, "coverage_pct": N },
  "recommendations": { "use": ["search", "explain", ...], "run": "tsift index ." },
  "reminders": ["index stale: run `tsift index .` before relying on tsift search/explain/graph (8 stale files); no summaries are cached, so run `tsift summarize --extract .` after the index is fresh when summary refs are needed"]
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
tsift summarize --extract <path> --max-file-tokens 12000 # override the configured per-file cap
tsift summarize --extract <path> --force # retry cached terminal failures for unchanged content
tsift summarize --stats             # summary totals, stale-file count, token savings
tsift summarize --json              # structured output
```

### Architecture

```
tsift summarize
├── extract (one-time, per file content hash)
│   ├── reads source + AST symbols from index.db
│   ├── resolves one extraction client before the file walk: Claude Code CLI for Bedrock/Vertex/Foundry hosts, direct Anthropic API when its configured key exists, otherwise an authenticated Claude Code CLI fallback
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

CREATE TABLE extraction_failures (
  file_path TEXT NOT NULL,
  content_hash TEXT NOT NULL,
  kind TEXT NOT NULL,              -- too_large | unparseable_response
  max_file_tokens INTEGER,         -- effective cap for too_large; NULL otherwise
  message TEXT NOT NULL,
  failed_at TEXT NOT NULL,
  PRIMARY KEY (file_path, content_hash)
);
```

### Extraction Protocol

1. Collect target files (from path arg or `--diff` against `git diff --name-only`; unborn HEAD falls back to untracked files only)
2. Claim the coarse `summaries.lock` sidecar so only one extractor mutates a cache at a time, then classify files by content hash and remove stale rows. Empty and ASCII-whitespace-only source files are not extraction candidates and never reach the model. Before extraction, remove cached terminal failures for files that are missing or no longer candidates, including legacy zero-byte failure rows. Fully cached runs require no model credentials. A cached `too_large` failure stops applying automatically when the effective `--max-file-tokens` / `.tsift/config.toml` cap rises above the cap that produced it.
3. If at least one cache miss needs extraction, resolve the transport once before the extraction walk. Prefer `claude -p` when a Claude Code hosted-provider flag is active; otherwise use the configured direct Anthropic API key when present, then fall back to an executable, authenticated `claude` on `PATH`. If neither is usable, exit nonzero with one actionable credential error and no per-file duplicates.
4. For each cache miss, load source + symbols from `index.db`
5. Build extraction prompt: source snippet + symbol list + "extract entities, relationships, 2-sentence summary"
6. Submit through the resolved client. Claude Code runs non-interactively with the configured model, no tools, safe mode, no session persistence, and JSON output so it inherits direct, Bedrock, Vertex, or Foundry authentication without loading project automation. Its response must include `result` plus measured `usage`; input usage sums uncached, cache-creation, and cache-read tokens. Missing or malformed usage fails extraction instead of recording false zeroes. Direct API responses still fail closed on non-2xx status before content parsing.
7. Parse each response and insert/update `summaries.db`. The parser accepts bare JSON or the first balanced JSON object after ordinary model preamble/fencing. Parse errors include the exact parser reason and a bounded response preview. Files over the cap and unparseable responses are cached by normalized path plus content hash as terminal failures, skipped on later runs, and retried after content changes, when `--force` is supplied, or when a raised cap invalidates `too_large`. A successful replacement clears failures for that file.
8. Before each model call, report `extracting N/total: <path>` on stderr so long runs remain visibly live. Then report files processed, entities found, measured tokens spent, terminal failures skipped, and estimated savings. A cached `too_large` skip recommends the minimum known `--max-file-tokens` value; `--force` is reserved for retrying other terminal failures.

### Token Savings Model

Without summarize: reading a 500-line file costs ~2000 tokens per context load.
With summarize: loading the cached summary costs ~50-100 tokens. Savings compound across repeated queries in a session.

`--stats` reports: total summaries, cached files, stale files, and estimated tokens saved across sessions.

### Boundary Rule

`tsift summarize` owns cached, pre-computed analysis that's deterministic after extraction. It does NOT:
- Run live LLM calls at query time (only explicit extraction invokes a model)
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
