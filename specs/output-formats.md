# tsift Spec — Output Formats & Envelopes

Part of the [tsift spec](../SPEC.md). See that index for the full command/spec map.

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
- Symbol-bearing preview items also promote the representative indexed AST span into a first-class `ast` artifact when the source file is readable and the index row has span columns. The artifact carries `artifact_kind=ast_span`, the stable `span-*` handle, node kind, byte/body spans, line/body-line ranges, Markdown span metadata where applicable, and direct `source-read`, body-window, and `symbol-read` expansion commands. Markdown representative spans additionally carry `markdown-ast --node <span-*>`, so a search result can jump from prose search to the selected heading/list/code block projection without re-scanning the whole document. Markdown code-block spans include parsed `embedded_symbols[]` for supported fence languages, and AST child facet matching can use those embedded symbol names/handles to navigate from a prose/fence result into the code island.
- Search preview reports include a merged `report.ranked` path alongside the compatibility `symbols` and `hits` lists. The ranking profile combines indexed symbol score, precise AST span presence, cached summary references, AST/traversal-neighborhood parent-child or embedded-code handles, and capped lexical file score; file-level lexical hits stay visible as fallback evidence but are weighted below precise symbol/span matches so broad prose matches do not drown out exact code spans. Each ranked row carries a stable `srnk-*` handle, source label (`symbol_span`, `symbol`, or `lexical_file`), score, expansion command, and scoring reasons.
- Search can narrow indexed symbol/AST previews with repeatable facet flags: `--lang`, `--kind`, `--node-kind`, `--section`, `--parent`, `--child`, `--fence-language`, `--list-depth`, and `--heading-level`. Scalar facets match symbol rows directly; AST facets resolve the representative span against the current index/source file so section path elements/handles, parent or direct-child names/handles, Markdown fence language, list depth, and heading level can be filtered before preview grouping. Active filters are echoed in `report.filters` and human preview output; file-level lexical hits remain visible as broader content fallback.
- `context-pack` loads tagpath ontology docs from `.naming/tags/*.md` when present and attaches compact `ontology_refs` to visible symbol refs and summary refs. Each ref carries a stable handle, canonical tag, markdown path, and optional title/domain metadata, while deliberately omitting ontology prose so agents can expand the tag document by path only when needed.
- When search preview mode sees repeated symbol hits that collapse to the same canonical `tag_alias`, it emits one family summary row with match/file counts plus a follow-up `expand` command keyed to that canonical tag family instead of repeating every surface spelling inline.
- When a search preview looks too broad for safe fan-out, the report includes a `scale_guard` with `high-hit` or `corpus-size` level, explicit corpus/tool-budget signals, and concrete `narrow_commands` to run before dispatching parallel agents. Envelope `follow_up` lists those narrowing commands before ordinary item expansion commands.
- JSON/terse/schema output in preview mode returns the same bounded preview report instead of the full raw payload; without these flags, the existing output formats remain unchanged.

`tsift token-savings --fixture <path>` is a CI-friendly report surface for preview compression contracts. The fixture lists per-command cases with raw symbol rows, compact tagpath families, and minimum savings thresholds; session-review cases can include raw `prompt_targets`, `sessions`, `commands`, `touched_files`, `touched_symbols`, `failures`, `guardrails`, and `largest_turns`, context-pack cases can include raw `next_context`, `diff`, `test`, and `log` input rows, source-read cases can include raw repeated source-file read rows with the compact `source-read` window plus required line anchors, and session-review/context-pack cases can include raw Markdown bodies with compact Markdown projection rows (`outline_nodes`, selected `mdast-*`/`span-*` handles, and an expansion command). Source-read fixture rows fail closed when a compact window hides a required anchor, and Markdown projection rows fail closed when they omit outline or selected-node handles, so full-file `cat`/`bat` reads, oversized `sed`/`head`/`tail` windows, and raw session-document markdown can prove token savings without losing the references that made the original read useful. That keeps the benchmark focused on the real transcript and handoff sections that dominate prompt volume, not only symbol-family compression. tsift serializes the raw rows and the compact envelope rows, then reports byte deltas, estimated token deltas using `ceil(utf8_bytes / 4)`, savings percentages, and pass/fail status per command. `--json` emits the report as structured data, `--fail-under` exits non-zero when any case misses its fixture threshold, and `tsift --envelope token-savings ...` wraps the same report in the common summary envelope.

`tests/exit_code.rs` runs the compiled `tsift token-savings --fixture ../tagpath/fixtures/tsift-token-savings.json --fail-under --json` path against tagpath's shared fixture and locks the current preview contract to `search`, `explain`, `session-review`, and `context-pack`, including the context-pack fail-under threshold for compact handoff previews. It also runs `fixtures/real-session-token-savings.json`, a tsift-owned benchmark derived from recent tsift/agent-doc transcripts, so `session-review`, `context-pack`, source-read rewrites, and Markdown projection rewrites keep proving large savings on realistic prompt-target, transcript, diff, test, build, install, push, raw session markdown, full-file `cat`/`bat`, and oversized `sed`/`head`/`tail` rows while retaining the resumable follow-up command and required line-anchor or selected-node surface.

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

`source-read` envelopes apply the schema-then-values transform by default. Source windows and Markdown projections contain dense repeated lists (`summary.metrics`, `report.preview`, `report.symbols`, `report.markdown.outline`) where record keys would otherwise dominate the response. The payload stays JSON, but homogeneous arrays use the columnar `{"_c":[...],"_r":[...]}` form described below. Non-envelope `source-read --json` keeps the command-specific object arrays unless `--schema` is passed explicitly.

### Command/Test-Run Envelopes

`tsift --envelope digest-runner ... --json` now wraps command-execution digests in a summary-first envelope for `test` and `log` runs.

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

## Ultra-Terse Mode

`tsift --ultra-terse` is a global flag that applies additional token reduction on top of `--terse` (it implies `--terse`). Targeted at agents operating under tight context budgets.

**Transforms applied (on top of terse key abbreviation):**

1. **Graph node/edge property stripping** — removes the `properties` field from objects detected as graph nodes (have `id` + `kind` + `label`) or graph edges (have `from_id` + `to_id` + `kind`), keeping only `id`, `kind`, `label` for nodes and `id`, `from_id`, `to_id`, `kind` for edges. Also strips `provenance` and `freshness` from graph edges.

2. **Edge kind abbreviation** — maps edge kind values (`k` field in graph edges) to short codes: `calls→c`, `defines→d`, `contains→ct`, `imports→i`, `mentions→m`, `mentions_concept→mc`, `mentions_entity→me`, `semantic_relation→sr`, `belongs_to→bt`, `scopes_context→sctx`, `scopes_source→ssrc`, `requests_context→rctx`, `explains_result→er`, `tagged_concept→tc`, `tagged_entity→te`, `related_concept→relc`, `handled_by→hb`, `defines_route→dr`, `handles_route→hr`, `targets→tgt`, `uses→u`, `parent→p`, `child→ch`, `enclosing_module→em`, `enclosing_section→es`, and more. Unknown kinds pass through unchanged.

3. **Snippet truncation** — `snippet`/`sn` string values are truncated to 80 characters with a `...` ellipsis suffix when the original exceeds 80 chars.

4. **Coverage snapshot compaction** — `SearchCoverageSnapshot` objects retain only `mode`, `total_sector_count`, and `dirty_sector_count`. Removes `active_rebuild`, `completed_dirty_sector_count`, `mounted_sector_count`, `rebuilding_sector_count`, `resumed_sector_count`, `reused_sector_count`.

5. **Edge index references** — in ultra-terse mode, `from_id`/`to_id` in edges are replaced with positional indices into the accompanying `nodes` array (`from`/`to` as integers). Edges whose endpoint IDs are not in the nodes array retain the original `from_id`/`to_id` string fields. Estimated 30-50% edge token reduction in graph-heavy envelopes.

**Expected savings:** ~30-50% token reduction over terse mode for graph-heavy and search-heavy responses.

```bash
tsift --ultra-terse search "main"          # ultra-terse JSON (implies --terse --json)
tsift --ultra-terse explain main            # stripped graph output
tsift --ultra-terse --envelope search "fn"  # envelope + ultra-terse
```

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
tsift --envelope source-read src/lib.rs    # source-read envelopes use columnar repeated lists by default
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

**Scope:** applies to all commands that emit file paths — `search`, `graph`, `explain`, `index`, `summarize`, `lint`, and JSON community member context. Human `communities` output still shows compact symbol names by default.

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

## Output Contract

Retrieval returns function-level results, not file-level:
- Function signature + file:line location
- 1-hop dependencies (callers/callees)
- 50-200 tokens per result vs. 2000+ for full-file reads
