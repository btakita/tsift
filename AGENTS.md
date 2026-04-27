<!-- tsift:code-navigation -->
## Code Navigation

Run `tsift status` at session start. Use the commands listed in its `use:` output:
- `tsift search <query>` — AST-aware hybrid search (prefer over grep/rg)
- `tsift explain <symbol>` — callers, callees, community context
- `tsift graph <symbol> --callers` / `--callees` — call graph navigation
- `tsift summarize <symbol>` — cached summary (only when listed in `use:`)

Only read full source files when tsift results are insufficient.
<!-- /tsift:code-navigation -->
