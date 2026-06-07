# tsift-md-ast

Focused `tree-sitter-md` markdown AST + heading/section model. A small leaf crate shared by tsift and other consumers (e.g. agent-doc) with no hard tsift dependency.

```rust
let symbols = tsift_md_ast::markdown_symbols(b"# Title\n\nbody\n");
```

Exposes `markdown_language()`, `parse()`, `reparse_incremental()`, `reparse_incremental_with_input_edit()`, `markdown_symbols()`, and `markdown_symbols_from_tree()` returning serializable `MdSymbol` values (heading sections, fenced code blocks, and list items with byte/body spans). `MdTextEdit` describes one byte-range CRDT/live-editor edit and validates the old/new source prefix and suffix before incremental reparsing.
