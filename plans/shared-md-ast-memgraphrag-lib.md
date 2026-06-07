# Shared markdown-AST (+ optional MemGraphRag) library

**Status:** tsift-side phases 1-2 shipped; agent-doc integration remains separate
**Backlog:** `#tsift-shared-md-ast-and-memgraphrag-lib`
**Context:** `agent-doc-bugs2.md` (#md-ast-document-model) wants `tree-sitter-md` AST + CRDT for the live document, MemGraphRag optional, via "a focused `tree-sitter-md` crate shared by both [agent-doc and tsift], no hard tsift dep."

## Premise (answered)

`tree-sitter-md` is **already** a tsift dependency and is actively used:
- `packages/tsift-graph/Cargo.toml`: `tree-sitter-md = "0.5"` (optional, `lang-markdown = ["dep:tree-sitter-md"]`, on by default).
- `packages/tsift-graph/src/lang.rs:91`: `Self::Markdown => tree_sitter_md::LANGUAGE.into()`, with heading→section symbol extraction (`extract_markdown_symbols`, `MarkdownHeading`, `markdown_zero_based_end_line`, ~lines 440–527).

So agent-doc can reuse tsift's markdown AST — but today that logic is welded into `tsift-graph` (which pulls the whole graph/index stack). agent-doc should not hard-depend on tsift.

## Goal

Extract a small leaf crate — `tsift-md-ast` — owning only:
- the `tree-sitter-md` parse (`parse(source) -> Tree`),
- the heading/section model (`MarkdownHeading`, section spans, zero-based line mapping),
- a stable, serializable AST/section view for consumers.

Both `tsift-graph` (depends down onto it for `Lang::Markdown`) and `agent-doc` (depends onto it for the live-document model) consume it. **No agent-doc → tsift dependency** — both depend on the shared leaf, mirroring the dependency-direction rule used for the proposed content-dedup ledger (`#7qky`).

## Phases

1. **Extract** `tsift-md-ast` leaf crate from `tsift-graph/src/lang.rs` markdown logic; `tsift-graph` depends on it; behavior/tests preserved (move the markdown extraction tests).
2. **Shipped**: stable serializable section/list/code-block symbols plus `MdTextEdit`, `reparse_incremental()`, and `reparse_incremental_with_input_edit()` for CRDT-backed live-document reparses.
3. **agent-doc integration** (agent-doc-side, separate session): agent-doc depends on `tsift-md-ast` for its document model; CRDT for the live doc; **MemGraphRag optional** (only when a project opts into the graph substrate).

## Notes

- Keep the crate dependency-light (tree-sitter + tree-sitter-md only) so agent-doc's hot path stays lean.
- MemGraphRag (the `#memgraphrag*` work in tsift) is optional and lives in tsift's graph stack, not in the shared leaf.
- Coordinate the agent-doc side via `agent-doc-bugs2.md` `#md-ast-document-model`; this plan owns the tsift-side extraction only.
