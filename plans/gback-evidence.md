# #gback Evidence

Date: 2026-05-26

Prompt: spike a real optional graph backend adapter only after SQLite gate evidence is fresh; compare Kuzu and FalkorDB behind `GraphStore` on projection writes/load, lock semantics, install portability, and parity; do not promote a read-only prototype unless it beats SQLite across every required workload.

## Refreshed Evidence Boundary

This spike uses the fresh `#gperf` and `#gfront` full-projection backend-eval runs instead of adding a new dependency in this cycle. Those runs show the current SQLite read path is not the hot path:

| Evidence | Finding |
| --- | --- |
| `plans/gperf-evidence.md` | Full-projection hotspots are source graph build, provider-neutral projection rows, and SQLite projection writes. The median full-projection `path_max_hops_512` probe on the synthetic deep-chain workload was sub-millisecond. |
| `plans/gfront-evidence.md` | Real and full-projection SQLite evidence/path probes are microsecond-scale; the approximately 1.1s prototype path probes are candidate read-only store behavior, not SQLite frontier behavior. |

## Adapter Comparison

| Dimension | FalkorDB adapter gate | Kuzu adapter gate |
| --- | --- | --- |
| Projection writes/load | Must write provider-neutral rows through a real optional adapter rather than replaying rows into the dependency-free prototype snapshot. | Must write provider-neutral rows through a real optional adapter rather than replaying rows into the dependency-free prototype snapshot. |
| Lock semantics | Must prove writer and read-only behavior for service-backed workflows, including local fallback semantics, without weakening SQLite's current old-or-new projection visibility. | Must prove embedded/native writer and read-only behavior without weakening SQLite's current old-or-new projection visibility. |
| Install portability | Must keep default `cargo build` / `cargo install --path .` service-free. | Must keep default `cargo build` / `cargo install --path .` free of required native Kuzu tooling. |
| Full parity | Must match SQLite signatures for every measured `GraphStore` operation on real, full-projection, high-degree, and deep-chain workloads. | Must match SQLite signatures for every measured `GraphStore` operation on real, full-projection, high-degree, and deep-chain workloads. |
| Promotion bar | Must beat SQLite on every required backend-eval workload and metric, including projection load/write cost. | Must beat SQLite on every required backend-eval workload and metric, including projection load/write cost. |

## Decision

Do not implement or promote a real Kuzu or FalkorDB adapter in this cycle. The fresh evidence points at projection construction/write cost before backend replacement, and the existing read-only prototypes are useful benchmark fixtures but are not production adapter proof.

The enforceable closeout is now `performance_gate.backend_adapter_spike` in `graph-db backend-eval`. It holds Kuzu and FalkorDB behind the same explicit requirements: real optional adapter, projection load/write proof, SQLite parity, lock behavior, install portability, and faster-than-SQLite results across every required workload. The conformance suite asserts that contract so a future adapter cannot be promoted from read-only prototype evidence alone.
