# Versions

tsift is private software, but its command surface is still versioned explicitly.

Canonical binary version source: `Cargo.toml` `package.version`. The CLI exposes the same value via `tsift --version`.

Use `BREAKING CHANGE:` prefix in version entries to flag incompatible changes.

## Unreleased

- Read-only `index.db` and `summaries.db` recovery is now WAL-aware end to end: when a live SQLite lock blocks reads and `-wal` / `-shm` sidecars are present, tsift copies that live sidecar state into the snapshot fallback instead of copying only the main `.db` file or waiting for a rollback-journal marker that never appears in normal WAL mode.
- `tsift status` / `tsift locks` now report WAL sidecar presence explicitly and distinguish `snapshot_fallback_wal` recovery from the older rollback-journal snapshot path, so lock diagnostics describe the real live lock mode instead of implying every fallback came from `*.db-journal`.
- Regression coverage now locks this behavior in the shared read-only helpers, the direct status/summary readers, and compiled CLI `status` plus `summarize --stats` flows under a live WAL writer.

## 0.1.21

- Plain `tsift search` on a workspace root no longer auto-creates `.tsift/index.db` when the workspace only has scoped `.tsift/indexes/<scope>/index.db` files. It now fails closed and requires the caller to pick `--scope <scope>` or `--federated`.
- The new workspace-search error lists both the available scope ids and the currently indexed scope ids, so agents can choose the right search target without guessing or mutating the workspace layout by accident.
- Regression coverage now locks this behavior in both the direct command path and the compiled CLI search surface.
- Read-only summary queries (`tsift summarize --stats`, `tsift summarize <symbol>`, `tsift summarize --file <path>`) now retry against a snapshot copy when a rollback-journal lock wedges the live `summaries.db`, instead of surfacing a raw `database is locked` failure.
- `tsift status` summary coverage checks now use that same resilient summary read path and expose `recovery: snapshot_fallback` / `summaries recovery: ...` diagnostics when they had to degrade off the live cache.
- Regression coverage now locks this behavior in the low-level summary reader, the direct summarize/status command paths, and the compiled CLI summarize surface.

## 0.1.20

- `tsift status` now treats workspace scoped indexes as authoritative whenever `.gitmodules` defines scopes, even if a shared `.tsift/index.db` also exists, so missing scoped DBs can no longer masquerade as a fresh workspace.
- Mixed root-plus-scoped workspace status now keeps reporting `workspace_scopes` and `missing_scopes`, and the top-level recommendation continues to point at `tsift index --workspace .` instead of the shared-root path.
- Regression coverage now locks this behavior in both the direct status helpers and the compiled CLI status surface.

## 0.1.19

- `tsift status`, `tsift search`, and the read-only graph query commands now resolve nested input paths against the nearest ancestor project root that already owns `.tsift/`, instead of treating subdirectories as brand-new projects.
- Nested-path query calls therefore reuse the existing root or scoped indexes and stop auto-creating stray subdirectory `.tsift/index.db` state during search autoindex.
- Regression coverage now locks this behavior in the shared path-resolution helper, the direct command paths, and the compiled CLI query/status surface.

## 0.1.18

- `tsift summarize --extract <path> --diff` now includes untracked files under the requested extract scope, instead of only re-extracting tracked paths reported by `git diff --name-only HEAD`.
- Diff extraction now skips deleted paths before the summarize walk, so `--diff` only feeds readable source files into the extraction batch.
- Regression coverage now locks this behavior in the direct summarize diff path and the compiled CLI summarize surface.

## 0.1.17

- `tsift graph`, `tsift communities`, `tsift path`, and `tsift explain` now fail closed on workspace roots that only have scoped `.tsift/indexes/<scope>/index.db` state, instead of pointing at a missing `.tsift/index.db` and hiding the real fix.
- The new error explicitly requires `--scope <scope>` and lists both the available scope ids and the currently indexed scopes, so agents can pick a valid workspace query target without guessing.
- Regression coverage now locks this behavior in both the direct command path and the compiled CLI query surface.

## 0.1.16

- `tsift status` no longer reports a partially indexed workspace as `fresh`. If some configured scoped `index.db` files are missing, full-workspace misses remain `index: missing` while partial workspaces surface as `index: stale` with explicit `missing_scopes`.
- Workspace status output and `--json` now list the missing scope ids directly, so agents can distinguish "files changed" from "this configured submodule has never been indexed yet."
- Regression coverage now locks this behavior in both the direct status helpers and the compiled CLI status surface.

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
