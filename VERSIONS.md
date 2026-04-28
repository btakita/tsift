# Versions

tsift is private software, but its command surface is still versioned explicitly.

Canonical binary version source: `Cargo.toml` `package.version`. The CLI exposes the same value via `tsift --version`.

Use `BREAKING CHANGE:` prefix in version entries to flag incompatible changes.

## 0.1.0

- Initial private versioned release surface for the tsift CLI.
- Commands available: `index`, `search`, `graph`, `communities`, `path`, `explain`, `edit`, `route`, `rewrite`, `sql`, `audit`, `summarize`, `lint`, `status`, `init`.
- Global output controls available: `--compact`, `--pretty`, `--terse`, `--schema`, `--absolute`, `--tabular`.
- Project setup includes Code Navigation instruction injection plus optional Codex auto-reindex hook install via `tsift init --codex`.
- `tsift search` now fast-fails on stale existing indexes and adds `--autoindex` for hook-like one-off recovery in unhooked sessions.
- Writable index updates now use a sibling `index.lock` sidecar so concurrent `tsift index` / `tsift search --autoindex` writers fail fast with a tsift-owned lock message instead of raw SQLite lock errors.
- Instruction version markers: `tsift init` now embeds `v=X.Y.Z` in the `<!-- tsift:code-navigation -->` opening marker. `tsift status` reports `instructions: current|stale|missing` and recommends `tsift init` when the installed version differs from the marker version. Pre-versioned markers (no `v=` attribute) are treated as stale.
