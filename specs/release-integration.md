# tsift Spec — Release, Audit, Hooks & Integration

Part of the [tsift spec](../SPEC.md). See that index for the full command/spec map.

## Release Workflow

tsift release automation is tag-driven:

- `push` of a `vX.Y.Z` tag runs the release workflow
- the workflow fails closed if the tag does not exactly match `Cargo.toml` `package.version`
- release verification includes `cargo clippy --workspace --all-targets -- -D warnings`, `cargo test --workspace`, and `cargo package -p <crate> --locked --allow-dirty --list` for every split Rust crate in dependency order
- successful tagged releases attach prebuilt archives plus `.sha256` checksum files to the matching GitHub Release
- prebuilt binaries are emitted for `x86_64-unknown-linux-gnu`, `aarch64-apple-darwin`, and `x86_64-pc-windows-msvc`; macOS x86_64 users install from crates.io with `cargo install tsift`
- the crates.io publish job is enabled with the `TSIFT_ENABLE_CRATES_PUBLISH=true` repo variable and authenticates via OIDC trusted publishing — it requests a GitHub `id-token` and exchanges it through `rust-lang/crates-io-auth-action` for a short-lived crates.io token, so there is no long-lived `CARGO_REGISTRY_TOKEN` secret to expire; it runs `cargo publish -p <crate> --locked --dry-run` immediately before each real publish after earlier dependency crates have landed, skips crate versions that already exist on crates.io so interrupted releases can resume, and retries crates.io rate limits. Each crate must already exist on crates.io with a Trusted Publisher configured (repository `<owner>/tsift`, workflow `release.yml`); bootstrap a brand-new crate's first version once before relying on trusted publishing

tsift's default lexical search adapter is maintained in-tree so crates.io publishing does not depend on the upstream git-only `github.com/rupurt/sift` project. The existing crates.io `sift` crate remains a different project and is intentionally not used.

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

The UI hook itself must not run the `tsift` binary. It starts a detached helper and returns immediately; that helper debounces briefly, takes a non-blocking workspace-wide single-flight lock, and runs both the freshness check and any required refresh at reduced CPU priority. Native tsift `index.lock` coordination remains the fallback on platforms without `flock`.

Workspace autoindex can be focused so large workspaces do not scan every scope on every prompt. Configure scope ids or relative submodule paths, then rerun `tsift init --codex --codex-workspace` to regenerate the helper:

```toml
[autoindex]
focus = ["agent-doc", "tsift"]
# Optional Linux taskset CPU list. Configure only CPUs reserved away from the UI.
cpu_affinity = "16-31"
```

`cpu_affinity` constrains every thread in the detached tsift process with `taskset -c`. Affinity alone does not create a non-UI processor: for hard isolation, reserve that CPU set at the operating-system or cgroup level and exclude Xorg/the compositor from it. Leave the setting unset when no CPU set has been reserved; the helper still runs at low CPU priority.

Each automatic refresh generation has one shared 120-second wall-clock budget by default (`TSIFT_AUTOINDEX_MAX_SECONDS`, with `0` disabling the budget). Every command receives only the generation's remaining time. GNU `timeout` first sends `TERM`, waits five seconds, then sends `KILL`. A timed-out worker releases both the hook `flock` and tsift's process-owned index lock; a later filesystem event or prompt can retry the incremental refresh.

An empty focus preserves workspace-wide warming. Focus only limits proactive background warming: `search`, `source-read`, `symbol-read`, and graph reads retain their synchronous per-target freshness checks, so an unfocused scope is refreshed before it is consumed.

### Search Rewrite (`PreToolUse`)

The `tsift-rewrite.sh` hook (`examples/hooks/tsift-rewrite.sh`) intercepts high-token shell commands and silently rewrites them to lower-context tsift flows:

- `rg ...` / `grep -r ...` → `tsift --envelope search ... --exact --budget normal`
- `git diff`, `git diff --cached`, commit-form `git show`, and simple `git log -p -1 ...` history review → `tsift diff-digest ...`; pathspecs become repeatable `--pathspec` filters, while blob/tree reads such as `git show HEAD:path` decline explicitly because they are not commit diffs
- whole or explicitly windowed transcript reads (`cat`, `bat`, `less`, `head -n`, `tail -n`, `sed -n`) over recognized agent-doc markdown sessions, Claude JSONL, or Codex JSONL → `tsift session-digest ...`, anchored to the transcript's owning repo or submodule root. Canonical `.claude/projects/...` and `.codex/sessions/...` paths identify their transcript source even when the bounded file prefix contains only queue or hook records. Generic `.log`, `.out`, `.output.txt`, and `.log.txt` inputs → `tsift log-digest ...`; indexed source inputs → exact `source-read --style window` ranges, including small explicit windows.
- `cargo test ...`, `pytest ...`, `python -m pytest ...` → `tsift --envelope digest-runner --kind test ...`
- `cargo build ...`, `cargo check ...`, `cargo clippy ...`, `cargo install ...` → `tsift --envelope digest-runner --kind log ...`

File-listing commands are not search rewrites. `rg --files ...`, `rg --type-list`, and `find ...` pass through so multiple roots, glob/predicate semantics, shell safety, ignore rules, and the original listing behavior are preserved instead of treating a root path as an exact search pattern. In hook/manual `tsift rewrite` protocol terms this is a no-rewrite exit: stdout stays empty, exit status is 1, and stderr carries a bounded reason plus guidance to run the original command unchanged.

Unsupported shell forms are explicit too. Commands with shell metacharacters such as pipes, redirection, or background operators are not rewritten; the no-rewrite response keeps stdout empty, exits 1, and writes a bounded stderr explanation instead of silently failing. In `--run` mode, no-rewrite exits still do not execute the original command; stderr tells the caller to run the original command directly if intended.

Recognized raw-read commands also retain their decline cause. Missing/unreadable files, unsupported input kinds, below-threshold whole-file reads, out-of-range windows, and source files without index coverage produce distinct messages; `~/...` inputs are expanded before classification. The whole-file threshold remains a cost gate, but an explicit `head`/`tail`/`sed` window already supplies a bound and is therefore rewritten even when it requests fewer than 80 lines.

The digest-runner path preserves the wrapped command's original exit status while replacing raw stdout/stderr with a summary-first envelope, bounded digest, and persisted transcript artifact, so failing tests/builds still fail closed and green runs do not inline raw logs. When RTK is installed, digest-runner probes `rtk rewrite <command>` and delegates supported generic command families to RTK's compact filters before wrapping the filtered output in tsift's envelope/artifact metadata.

**Claude Code hook** (`.claude/settings.json`):

```json
{
  "hooks": {
    "PreToolUse": [
      { "matcher": "Bash", "command": "examples/hooks/tsift-rewrite.sh" }
    ]
  }
}
```

**Harness-equivalent for Codex/OpenCode:** These harnesses do not support PreToolUse hooks. Instead, use `tsift rewrite --run '<command>'` which executes the rewritten command directly, preserving exit status and emitting the same envelope output. OpenCode users can install the `/tsift-rewrite-run` command shortcut via `tsift init --opencode`.

Harnesses that do not expose Claude-style `PreToolUse` hooks can still reuse the same rewrite path manually via `tsift rewrite --run '<command>'`. This is the explicit low-token path for Codex/OpenCode broad search, raw session/transcript/log reads, git diff/show/log patch review, cargo/pytest tests, and cargo build/check/clippy/install output. In `--run` mode, tsift executes the rewritten command directly instead of only printing it, preserves the rewritten command's exit status, and emits the same envelope search previews and digest-runner artifact envelopes by default.

Global structured-output flags are forwarded into the rewritten tsift command and deduplicated when the rewrite already chose an envelope. That means callers can still layer `--pretty`, `--terse`, or `--schema` onto the default summary-first execution output, for example:

- `tsift --pretty rewrite --run 'cargo test --manifest-path Cargo.toml'`
- `tsift --schema rewrite --run 'cargo build --manifest-path Cargo.toml'`
- `tsift rewrite --run 'cargo install --path . --force'`

Those commands emit the same `digest-runner` JSON envelope that `tsift --envelope digest-runner ... --json` uses internally, so agent-doc or other harnesses get bounded execution output without depending on shell-hook rewriting. If RTK is available and supports the wrapped command, `report.filter = {tool:"rtk", command:"..."}` identifies the delegated compact filter.

### RTK Output Filtering (`PreToolUse`)

The `tsift-rewrite.sh` hook (`examples/hooks/tsift-rewrite.sh`, phase 2) routes verbose tsift commands through RTK for output capping when RTK is installed. Commands routed: `communities`, `explain`, `graph`, `index`, `search`. Non-verbose commands (`status`, `init`, `route`, `sql`) pass through unchanged.

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

## Tagpath integration

Since 0.1.47, `tsift search` auto-detects a [tagpath](https://github.com/btakita/tagpath) index at the project root (`.naming/index.json`) and annotates each `SymbolHit` with a stable `tagpath_handle` field (`mem:<sha256[0..16]>`). Handles are content-addressable: ordinary edits that add a sibling member to a family do not change the family handle, so consumers can cite citations across edits.

### Detection

- The adapter walks up from the search path looking for `.naming.toml`. If none is found, the adapter returns `Missing` and tsift falls back to its native AST extraction with no annotation.
- If an index is found, `tagpath::index::check` runs to decide freshness. Fresh indexes are loaded and used; stale indexes log a `tagpath_index_stale: true` diagnostic to stderr and fall back to live extraction (no `tagpath_handle` is emitted).
- The strict-mode flag below converts the stale fallback into a hard error.

### Flags

- `--no-tagpath` — skip the lookup entirely (no annotation, no diagnostic). Useful for benchmarks and for debugging the native extraction path.
- `--tagpath-strict` — fail closed when the index is present but stale. Use this in CI / hook contexts where silent fallback would be a regression.

### `tagpath_handle` semantics

- The field is `Option<String>` and serializes only when present (`#[serde(skip_serializing_if = "Option::is_none")]`). Consumers that already know the field shape can rely on `tagpath_handle` being either `mem:...` or absent.
- Handle derivation lives in tagpath; see [`src/tagpath/SPEC.md` §15](../tagpath/SPEC.md#15-consumer-contract-tsift--agent-doc--external) for the wire and freshness contract.
- When more than one `symbol_info` row shares a name across files (e.g. two `main` definitions across `bin/foo/main.rs` and `bin/bar/main.rs`), surfaces with file or edge evidence probe every candidate row instead of trusting the first `symbol_info` row. This avoids dropping handles when the first-by-`(file, line)` file lives outside the tagpath walk (vendored, generated, or skipped directory), and lets graph/community outputs prefer the candidate backed by local evidence.
- Callee-edge annotation for `tsift graph` and `tsift explain` resolves each edge with its `caller_file` context instead of sharing one handle per callee name. If multiple indexed files define the callee name, tsift prefers a definition in the caller's file, then one whose file shares the caller's Louvain community evidence, and otherwise falls back to the first resolving handle.
- Community member annotation carries bounded edge refs plus selected `file`/`line` context into JSON output. If multiple indexed files define the same member name, tsift prefers a unique tagpath candidate, then edge-file evidence, then community-file evidence. It no longer assigns a tagpath handle by name-only first-row fallback; unresolved or evidence-resolved duplicate members are reported under `community_diagnostics.ambiguous_members`.
- Scoped `communities` and `explain` community annotation use the scope source root as the tagpath adapter project root, so per-submodule `.naming.toml` / `.naming/index.json` files resolve member handles even when the workspace root has no tagpath project.
- `tsift search --scope <name>` and inferred-scope search paths use the scope's `source_root` as the tagpath adapter project root, so a submodule that owns its own `.naming.toml` / `.naming/index.json` resolves `tagpath_handle` even when the workspace root has no tagpath project.
- `tsift search --federated` runs the tagpath annotation pass **per scope** inside `federated_symbol_search`, using each submodule's `source_root` as the adapter project root. Federated workspaces where each submodule owns its own `.naming.toml` and `.naming/index.json` resolve `tagpath_handle` for every per-scope hit; the workspace root usually has no tagpath project of its own and would otherwise produce a uniform `Missing` adapter load. The merged diagnostic reports `loaded=true` when any scope loaded and `stale=true` with the first scope's stale reason when any scope was stale.
- **Stale-index diagnostic surfaces in structured output**: when any `annotate_*_with_tagpath` helper reports `stale=true`, the JSON response from `tsift search`, `tsift path`, `tsift explain`, `tsift graph`, `tsift communities`, and the search/explain budget-mode reports adds a top-level `tagpath_index_stale: true` and `tagpath_stale_reason: <reason>` pair. The existing stderr `tagpath_index_stale: …` log line is preserved. `--no-tagpath` suppresses both the stderr line and the new JSON fields. JSON consumers can decide to re-run with `--tagpath-strict` or trigger a rebuild from the structured response without scraping stderr.

### Watch integration (deferred)

The current adapter is a one-shot loader; it does not subscribe to `tagpath watch`. A follow-up will add an on-demand `tagpath watch --once` refresh and (eventually) a long-running NDJSON subscription for server-mode tsift.

### Module layout

- `src/tagpath_adapter.rs` — `try_load`, `TagpathAdapter`, `LoadResult`, `HandleResolution`. Public so other tsift commands can opt into the same lookup surface as they wire it through.
- `src/main.rs::annotate_hits_with_tagpath` — annotation helper used by `cmd_search_with_budget`.
- `src/index.rs::SymbolHit::tagpath_handle` — the citation field.

## What NOT to build

- Visualization (Mermaid, HTML) — leave to graphify
- Full LSP-level type inference — diminishing returns
- Embedding model hosting — use external API or lightweight local model (all-MiniLM-L6-v2)
- Dynamic grammar loading (until binary size exceeds ~50MB)
- Live LLM calls at query time in `tsift summarize` — extraction is batch-only
