# tsift

Token-efficient CLI for code agents — AST-aware search, call-graph queries, batch editing, SQL introspection, and model routing.

## Agent instructions

- **Normative spec:** [`SPEC.md`](SPEC.md) (index) and its [`specs/*.md`](specs/) siblings.
- **Change history:** [`VERSIONS.md`](VERSIONS.md). Canonical version: `Cargo.toml` `package.version` (== `tsift --version`).
- **Standalone checkout** (no superproject skill present): use `tsift --help` / subcommand `--help` plus `SPEC.md`/`VERSIONS.md` as the source of truth.
- **Develop:** `make check` (clippy + full suite) then `cargo install --path .`.
