//! `tsift-sim-world` is a deterministic test-harness crate.
//!
//! It carries no production surface of its own — the SimWorld harness and its
//! seeded-corpus / named-edge-trace tests live in `tests/sim_world.rs`, where
//! `tsift` and `tsift-cli` are available as dev-dependencies. Keeping the
//! harness in an integration test (rather than a `#[cfg(test)]` module inside
//! root `tsift`) avoids the `tsift -> tsift-sim-world -> tsift` lib cycle that
//! a re-export would create, while still running under `cargo test --workspace`
//! (and therefore `make check`).
