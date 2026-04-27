# Plan: `tsift status` command

One-shot health check that reports index freshness, summary cache availability, and a machine-parseable `use:` list so the agent knows which tsift commands are worth calling this session.

## Output format

```
index: fresh (last indexed 2m ago, 0 stale files)
summaries: 142/200 files cached (71%), 12 stale
recommendations:
  use: search, explain, graph, summarize
  run: tsift summarize --extract --diff src/  (12 stale files)
```

When no index exists:
```
index: missing
summaries: unavailable (no index)
recommendations:
  use: (none — run tsift index first)
  run: tsift index .
```

When index exists but no summaries:
```
index: fresh (last indexed 5m ago, 0 stale files)
summaries: none (run tsift summarize --extract to build cache)
recommendations:
  use: search, explain, graph
  run: tsift summarize --extract src/
```

## Implementation

### 1. New `Status` subcommand in `src/main.rs`

```rust
/// Report index + summary status and recommended commands for this session
Status {
    /// Path to the codebase (defaults to current directory)
    #[arg(default_value = ".")]
    path: PathBuf,
    /// Output as JSON
    #[arg(long)]
    json: bool,
}
```

### 2. New `src/status.rs` module

- `check_index(path)` — opens `symbols.db`, counts total files, runs `index --check` logic to count stale files, reports last index time
- `check_summaries(path)` — opens `summaries.db` if it exists, counts cached summaries vs total indexed files, counts stale (content hash mismatch)
- `build_recommendations(index_status, summary_status)` — determines which commands are useful:
  - No index → `use: (none)`, recommend `tsift index .`
  - Index but no summaries → `use: search, explain, graph`, recommend `tsift summarize --extract`
  - Index + summaries → `use: search, explain, graph, summarize`, recommend `--extract --diff` if stale > 0
- `StatusReport` struct with JSON serialization

### 3. Update `tsift init` directive

Change the injected AGENTS.md section to reference `tsift status`:

```markdown
## Code Navigation

Run `tsift status` at session start. Use the commands listed in its `use:` output:
- `tsift search <query>` — AST-aware hybrid search (prefer over grep/rg)
- `tsift explain <symbol>` — callers, callees, community context
- `tsift graph <symbol> --callers` / `--callees` — call graph navigation
- `tsift summarize <symbol>` — cached summary (only when listed in `use:`)

Only read full source files when tsift results are insufficient.
```

### 4. Tests

- Status with no index → reports missing, recommends `tsift index`
- Status with fresh index, no summaries �� lists search/explain/graph
- Status with fresh index + summaries �� lists all four commands
- Status with stale index → recommends reindex
- Status with stale summaries → recommends `--extract --diff`
- JSON output matches expected schema

### 5. Update SPEC.md + CLAUDE.md

Add `tsift status` to the command table and add a "Status" section to SPEC.md.

## Scope

~200-300 lines of new code. Reuses existing `index::check` and `summarize::stats` logic. No new dependencies.
