# ghoppolicy - hop-cap promotion policy evidence

Goal: close `#ghoppolicy` by keeping graph path max-hop increases opt-in
until repeated evidence proves higher caps are both useful and flat enough.

## Policy

The user-facing graph path default stays at 64 hops. Higher tiers remain
benchmark evidence until `perf_gate::evaluate_hop_cap_promotion` promotes a
candidate tier.

Promotion requires at least three SQLite samples for all required workloads:

- `real`
- `full_projection`
- `synthetic_deep_chain`

For every workload, backend-eval must expose duration and row metrics for the
64-hop baseline plus 128, 256, and 512-hop candidate tiers. A candidate tier
must stay within the allowed latency-regression budget and return useful rows.
For `synthetic_deep_chain`, useful rows must exceed the 64-hop row median.

## Implementation

The existing gate already enforces the policy in `src/perf_gate.rs`:

- `HOP_CAP_CURRENT_DEFAULT = 64`
- `HOP_CAP_CANDIDATE_TIERS = [128, 256, 512]`
- `HOP_CAP_REQUIRED_WORKLOADS = ["real", "full_projection", "synthetic_deep_chain"]`
- `evaluate_hop_cap_promotion` blocks missing full-projection samples, missing
  row metrics, insufficient sample counts, latency regressions, and deep-chain
  candidate tiers that do not return more rows than the 64-hop baseline

This cycle tightened the backend-eval report contract test so
`performance_gate.hop_cap_promotion.required_metrics` must include duration and
row metrics for every 128/256/512 tier across every required workload.

## Fresh Samples

The current evidence reuses the three fresh full-projection backend-eval samples
captured after the latest installed binary in this worktree:

```bash
tsift graph-db --json backend-eval --full-projection > target/perf/gsqlstage-full-projection-1.json
tsift graph-db --json backend-eval --full-projection > target/perf/gsqlstage-full-projection-2.json
tsift graph-db --json backend-eval --full-projection > target/perf/gsqlstage-full-projection-3.json
```

Median SQLite hop metrics across the three samples:

| Workload | Hop cap | Median duration (us) | Median rows |
| --- | ---: | ---: | ---: |
| real | 64 | 85 | 2 |
| real | 128 | 38 | 2 |
| real | 256 | 33 | 2 |
| real | 512 | 31 | 2 |
| full_projection | 64 | 71 | 2 |
| full_projection | 128 | 35 | 2 |
| full_projection | 256 | 31 | 2 |
| full_projection | 512 | 30 | 2 |
| synthetic_deep_chain | 64 | 89 | 65 |
| synthetic_deep_chain | 128 | 139 | 129 |
| synthetic_deep_chain | 256 | 267 | 257 |
| synthetic_deep_chain | 512 | 530 | 513 |

The deep-chain rows are useful for every higher tier, but latency is not flat:
the 512-hop median is 530us versus the 64-hop median of 89us. Under the current
10% regression budget, the gate must keep the default at 64.

## Verification

- `cargo test -q graph_db_backend_eval_benchmarks_candidate_stores_against_sqlite`
- `cargo test -q hop_cap_gate`
- `cargo build`
- `make check`
- `cargo install --path .`

## Verdict

`#ghoppolicy` is closed. The default remains 64 hops; higher caps stay opt-in
and evidence-only until repeated real, full-projection, and deep-chain samples
pass the useful-row and latency gate.
