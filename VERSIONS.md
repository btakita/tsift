# Versions

tsift is private software, but its command surface is still versioned explicitly.

Canonical binary version source: `Cargo.toml` `package.version`. The CLI exposes the same value via `tsift --version`.

Use `BREAKING CHANGE:` prefix in version entries to flag incompatible changes.

## 0.1.15

- `tsift index --submodule <name>` now uses the same strict workspace scope resolution as `--scope`, so unknown selectors fail closed instead of indexing `root/<name>` into an unreachable scoped database.
- Ambiguous duplicate leaf-name selectors now fail closed for submodule indexing too, requiring the concrete scope id when `.gitmodules` contains colliding leaf names.
- Regression coverage now locks this behavior in both the direct `cmd_index` path and the compiled CLI index surface.

## 0.1.14

- `tsift status` now detects workspace-only indexes under `.tsift/indexes/<scope>/index.db` instead of reporting `index: missing` whenever the root `.tsift/index.db` is absent.
- Workspace status output now reports the indexed scopes explicitly, aggregates their freshness into the top-level `index` state, and recommends `tsift index --workspace .` / `tsift init --workspace` for workspace roots.
- Regression coverage now locks this behavior in both the direct status helpers and the compiled CLI status surface.

## 0.1.13

- Workspace scope identifiers now stay unique even when `.gitmodules` contains duplicate trailing directory names. Unique leaves still use the short leaf name (for example `alpha`), but duplicate leaves promote to the full submodule path (for example `pkg/app/foo`, `vendor/foo`) so indexing and scoped search no longer collide onto the same `index.db`.
- Ambiguous legacy leaf selectors now fail closed and list the concrete scope ids to use, instead of silently resolving to whichever duplicate scope happened to win first.
- Regression coverage now locks this behavior in config parsing, in-process workspace search, workspace indexing, and the compiled CLI search surface.

## 0.1.12

- Workspace `tsift summarize --extract ...` now resolves symbol context per extracted file, so files under `.tsift/indexes/<scope>/index.db` use the matching scoped index instead of whichever workspace index appears first.
- Summarize symbol preload now uses exact normalized file matches instead of suffix matching, preventing same-path collisions across scoped indexes and locking the prompt context to the intended file.
- Regression coverage now locks this behavior in the direct summarize helpers, the workspace summarize command path, and the compiled CLI summarize surface.

## 0.1.10

- `tsift summarize --stats`, `tsift summarize <symbol>`, and `tsift summarize --file <path>` now fail closed when `.tsift/summaries.db` is absent and otherwise open the summary cache read-only, so lookup paths no longer create or contend on the cache DB.
- Regression coverage now locks this behavior in both the direct `cmd_summarize` path and the compiled CLI summarize surface.

## 0.1.11

- `tsift summarize --extract <relative>` now resolves the walked extraction path against `--path` / the canonical project root instead of the caller's current working directory, so batch extraction targets the intended repo even when the CLI runs from elsewhere.
- Regression coverage now locks this behavior in both the helper-level summarize path resolution and the compiled CLI summarize surface.

## 0.1.9

- `tsift lint --index .tsift/indexes` now treats the scoped-index directory itself as a valid discovery root, so explicit per-submodule linting no longer ignores every `index.db`.
- Regression coverage now locks this behavior in both the helper-level entity discovery path and the compiled CLI lint surface.

## 0.1.8

- `tsift lint` now opens discovered `index.db` files through the shared read-only path with rollback-journal snapshot fallback, so markdown linting stays available while a live writer holds the database lock.
- Regression coverage now locks this behavior in both the helper-level entity-loading path and the compiled CLI lint surface.

## 0.1.7

- `tsift lint` now auto-discovers live `index.db` files from the nearest ancestor `.tsift` root, including scoped `.tsift/indexes/*/index.db` layouts, instead of probing the retired `symbols.db` paths.
- Regression coverage now locks this behavior in both the helper-level discovery path and the compiled CLI lint surface.

## 0.1.6

- `tsift search --scope <name>` now fails closed when the named submodule does not exist, and reports the available workspace scopes instead of silently falling back to a full-workspace lexical search.
- Regression coverage now locks this behavior in both the direct `cmd_search` path and the compiled CLI integration test surface.

## 0.1.5

- `tsift communities` now opens `index.db` through the same read-only path as `graph`, `path`, and `explain`, so it no longer acquires the `index.lock` writer sidecar for a read-only graph query.
- Regression coverage now holds a live writer lock and asserts that both the in-process command path and the compiled CLI still succeed for `tsift communities`.

## 0.1.4

- `tsift index --prune` now falls back to the same full file-mtime scan as normal incremental indexing, so file edits inside unchanged directories are still detected correctly.
- The `--prune` flag remains in place as a compatibility surface and reports prune stats, but active subtree skipping is suspended until tsift has a sound invalidation model that cannot miss in-place file edits.

## 0.1.3

- `tsift index` now records non-fatal warnings when a changed file cannot be read or when symbol/call extraction fails, instead of silently swallowing those `.ok()` paths.
- Those warnings are emitted on stderr from shared index-update flows and also carried in the structured `IndexSummary`, so manual indexing and search autoindex no longer hide partial extraction failures.

## 0.1.2

- Writable `index.db` opens now set and verify `PRAGMA wal_autocheckpoint=256`, so routine tsift writes checkpoint the WAL on an explicit budget instead of relying on SQLite defaults.
- Regression coverage now asserts the busy timeout, WAL journal mode, and explicit auto-checkpoint setting together.

## 0.1.1

- `tsift search --timeout` now runs the bounded sift search in an internal helper process and kills that worker on timeout, so timed-out searches no longer keep burning CPU in detached threads.
- `--timeout 0` still keeps search in-process for long-running sessions that explicitly opt out of the timeout.

## 0.1.0

- Initial private versioned release surface for the tsift CLI.
- Commands available: `index`, `search`, `graph`, `communities`, `path`, `explain`, `edit`, `route`, `rewrite`, `sql`, `audit`, `summarize`, `lint`, `status`, `init`.
- Global output controls available: `--compact`, `--pretty`, `--terse`, `--schema`, `--absolute`, `--tabular`.
- Project setup includes Code Navigation instruction injection plus optional Codex auto-reindex hook install via `tsift init --codex`.
- `tsift search` now fast-fails on stale existing indexes and adds `--autoindex` for hook-like one-off recovery in unhooked sessions.
- Writable index updates now use a sibling `index.lock` sidecar so concurrent `tsift index` / `tsift search --autoindex` writers fail fast with a tsift-owned lock message instead of raw SQLite lock errors.
- Instruction version markers: `tsift init` now embeds `v=X.Y.Z` in the `<!-- tsift:code-navigation -->` opening marker. `tsift status` reports `instructions: current|stale|missing` and recommends `tsift init` when the installed version differs from the marker version. Pre-versioned markers (no `v=` attribute) are treated as stale.
