# tsift-surrealdb

Optional SurrealDB graph store backend spike for tsift.

This crate is intentionally excluded from the default workspace and is only
compiled when the `backend-surrealdb` feature is enabled or when this manifest is
tested directly. It uses the stable SurrealDB Rust SDK 2.x embedded SurrealKV
engine to write provider-neutral graph projection rows to a file-backed store
while preserving the existing `GraphStore` contract.
