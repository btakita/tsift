# tsift

Token-efficient CLI for code agents — AST-aware search, call-graph queries, batch editing, SQL introspection, and model routing.

## Agent instructions

The **primary agent-facing instructions live in the tsift skill**, [`.claude/skills/tsift/SKILL.md`](../../.claude/skills/tsift/SKILL.md), which carries the full command surface and dispatches to its `runbooks/` (command reference, conventions, hook integration, internals, search strategies). This file stays lean and defers to that skill so instructions are not duplicated.

- **Normative spec:** [`SPEC.md`](SPEC.md) (index) and its [`specs/*.md`](specs/) siblings.
- **Change history:** [`VERSIONS.md`](VERSIONS.md). Canonical version: `Cargo.toml` `package.version` (== `tsift --version`).
- **Standalone checkout** (no superproject skill present): use `tsift --help` / subcommand `--help` plus `SPEC.md`/`VERSIONS.md` as the source of truth.
- **Develop:** `make check` (clippy + full suite) then `cargo install --path .`.

The Code Navigation block below is managed by `tsift init` (versioned markers) — do not hand-edit it; re-run `tsift init` to refresh. It is a router: the command detail it defers to lives in [`.agent/runbooks/code-navigation.md`](.agent/runbooks/code-navigation.md), generated and versioned by the same command. `CLAUDE.md` is `@AGENTS.md` and deliberately carries no copy of either.

<!-- tsift:code-navigation v=0.1.84 -->
## Code Navigation

Run `tsift status` at session start from the owning repo root. If the task or file lives under a git submodule (for example `src/tsift/...`), switch to that submodule root first so the harness loads the narrower local instructions and repo state instead of the superproject root. `tsift status` repairs the `.tsift/` index state it owns and never rewrites tracked files (`--no-fix` skips even that). If status reports stale or missing instructions, run `tsift init` to refresh the tracked Code Navigation block and runbook; it names every tracked file it rewrites or moves. When the harness cannot perform write commands, ask the user to run the printed `run:` command instead.

Prefer tsift envelopes over raw reads:
- `tsift --envelope search <query>` instead of `grep`/`rg`
- `tsift --envelope source-read <file>` / `tsift --envelope symbol-read <symbol>` instead of raw `cat`/`head`/`tail`/`sed`/`less` source reads
- `tsift --envelope explain <symbol>` and `tsift graph <symbol> --callers` / `--callees` for call graphs
- `tsift diff-digest [path]` (`--pathspec <pathspec>` to preserve scoped reviews) instead of `git diff`, commit-form `git show`, or patch-style `git log`; blob-form `git show <rev>:<path>` stays a raw object read
- `tsift --envelope session-review <path>` / `tsift --envelope context-pack <path>` instead of replaying long session docs or transcripts
- raw-read rewrites route recognized session docs/transcripts to `tsift session-digest --input <path>` and captured logs to `tsift log-digest --input <path>`
- `tsift --envelope digest-runner --kind test|log --path . --shell-command '<command>'` instead of raw test/build output

Command detail lives in [`.agent/runbooks/code-navigation.md`](.agent/runbooks/code-navigation.md) — budgets, `tsift workflow search`, `report.scale_guard` handling, the harness rewrite path for `PreToolUse`-less harnesses, and Codex/OpenCode integration. `tsift init` writes and versions that runbook alongside this block, so it is present in every initialized checkout; read it before broad exploration instead of expanding this block. A repository that also ships a current `.claude/skills/tsift/SKILL.md` should use that skill as the deeper source.

For local verification, run `make check` before committing. After local changes, check the latest GitHub Actions CI run with `gh run list --limit 1` and fix any failing tests before calling the work complete.

Only read full source files when tsift results are insufficient.
<!-- /tsift:code-navigation -->
