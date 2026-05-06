<!-- tsift:code-navigation v=0.1.34 -->
## Code Navigation

Run `tsift status` at session start from the owning repo root. If the task or file lives under a git submodule (for example `src/tsift/...`), switch to that submodule root first so the harness loads the narrower local instructions and repo state instead of the superproject root.

Use the commands listed in its `use:` output:
- `tsift search <query>` — AST-aware hybrid search (prefer over grep/rg)
- `tsift explain <symbol>` — callers, callees, community context
- `tsift graph <symbol> --callers` / `--callees` — call graph navigation
- `tsift summarize <symbol>` — cached summary (only when listed in `use:`)

Prefer bounded digest commands over raw transcript, diff, and verbose-log reads:
- `tsift session-digest <file>` / `tsift session-review <path>` / `tsift session-review --next-context <path>` instead of replaying long session docs, JSONL transcripts, or agent-doc runtime logs with `cat`, `tail`, or `sed`.
- `tsift diff-digest [path]` (`--cached`, `--revision <rev>`) instead of `git diff`, `git show`, or patch-style `git log`.
- `tsift test-digest --path .` / `tsift log-digest --path .` for noisy test/build/install output, or let the rewrite/hooks wrap `cargo test`, `pytest`, and verbose cargo commands for you.

Only read full source files when tsift results are insufficient.
<!-- /tsift:code-navigation -->
