# Vendored tree-sitter-jai

Generated `src/parser.c`, `src/scanner.c`, and `src/tree_sitter/*.h` from
<https://github.com/constantitus/tree-sitter-jai> at commit
`96440b0f78a45bed1668d5693f26c0d30b4d1a84` (2026-09-14), parser ABI 15.
Licensed MIT-0 (see `LICENSE`). Jai has no grammar crate on crates.io, so
`packages/tsift-graph/build.rs` compiles these sources when `lang-jai` is on.

To update: copy the same files from a newer upstream commit, record the commit
here, and run `cargo test -p tsift-graph jai`.
