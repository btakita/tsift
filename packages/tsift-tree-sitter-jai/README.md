# tsift-tree-sitter-jai

[Jai](https://en.wikipedia.org/wiki/Jai_(programming_language)) grammar for
[tree-sitter](https://tree-sitter.github.io/), packaged for
[tsift](https://github.com/btakita/tsift).

This crate is tsift's packaging of
[constantitus/tree-sitter-jai](https://github.com/constantitus/tree-sitter-jai)
at commit
[`96440b0`](https://github.com/constantitus/tree-sitter-jai/tree/96440b0f78a45bed1668d5693f26c0d30b4d1a84)
(2026-09-14, parser ABI 15). Jai has no grammar crate on crates.io, so tsift
ships the generated `src/parser.c`, the external `src/scanner.c`,
`src/node-types.json`, and `src/tree_sitter/*.h` unchanged from that commit and
compiles them in `bindings/rust/build.rs`. Keeping the 29 MB `parser.c` here
means a plain `tsift` build never downloads or compiles it; tsift enables it
only behind its opt-in `lang-jai` feature (part of `all-languages`).

It exposes the standard tree-sitter grammar crate API, `LANGUAGE` and
`NODE_TYPES`:

```rust
let mut parser = tree_sitter::Parser::new();
parser.set_language(&tsift_tree_sitter_jai::LANGUAGE.into())?;
```

tsift depends on it as `tree-sitter-jai = { package = "tsift-tree-sitter-jai", ... }`,
so if upstream publishes a `tree-sitter-jai` crate this one will be retired in
its favour with a one-line dependency change.

## Updating

Copy `src/parser.c`, `src/scanner.c`, `src/node-types.json`, and
`src/tree_sitter/*.h` from a newer upstream commit, record the commit above,
and run `cargo test -p tsift-tree-sitter-jai` and
`cargo test -p tsift-graph --features lang-jai jai`.

## License

MIT-0, the same as upstream. See [`LICENSE`](LICENSE).
