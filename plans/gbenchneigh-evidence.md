# #gbenchneigh evidence

Goal: refresh `fixtures/graph-db-performance-history.json` with fresh
full-projection rows that keep `neighborhood`, `path_max_hops`, evidence,
conflict-matrix, and dispatch-trace metrics together for future graph-backend
and hop-cap decisions.

## Commands

The samples used the installed `tsift 0.1.62` binary from the tsift submodule
and targeted the agent-loop superproject:

```bash
for sample in 1 2 3; do
  tsift graph-db --path /home/brian/work/btakita/agent-loop \
    --json backend-eval --full-projection \
    > "target/perf/gbenchneigh-sample-${sample}.json"
done

tsift metric-digest --input fixtures/graph-db-performance-history.json --json \
  > target/perf/gbenchneigh-metric-digest.json
```

Raw reports:

- `target/perf/gbenchneigh-sample-1.json`
- `target/perf/gbenchneigh-sample-2.json`
- `target/perf/gbenchneigh-sample-3.json`
- `target/perf/gbenchneigh-metric-digest.json`

## Fixture update

Three new runs were appended to
`fixtures/graph-db-performance-history.json`:

- `agent-loop-gbenchneigh-full-projection-2026-05-26-sample-1`
- `agent-loop-gbenchneigh-full-projection-2026-05-26-sample-2`
- `agent-loop-gbenchneigh-full-projection-2026-05-26-sample-3`

Each row carries the full `backend-eval --full-projection` metric map
(1,030 numeric metrics), `workload="full-projection"`,
`scope="agent-loop-superproject"`, `backend="sqlite"`, and
`projection_mode="full"`. Samples 1 and 2 recorded `cache_state="miss"`;
sample 3 recorded `cache_state="hit"`.

## Focused metrics

| Metric | Samples (us) | Median (us) |
| --- | ---: | ---: |
| `real.sqlite.total_duration_micros` | 58,284,228 / 32,395,060 / 40,866,128 | 40,866,128 |
| `full_projection.sqlite.total_duration_micros` | 22,730,509 / 23,281,343 / 21,384,931 | 22,730,509 |
| `real.sqlite.neighborhood.duration_micros` | 42,408 / 40,790 / 42,486 | 42,408 |
| `full_projection.sqlite.neighborhood.duration_micros` | 47,299 / 46,915 / 47,234 | 47,234 |
| `full_projection.sqlite.path_max_hops.duration_micros` | 65 / 74 / 68 | 68 |
| `full_projection.sqlite.path_max_hops_512.duration_micros` | 27 / 30 / 16 | 27 |
| `full_projection.sqlite.evidence.duration_micros` | 967 / 974 / 793 | 967 |
| `full_projection.sqlite.conflict_matrix.duration_micros` | 631,437 / 654,459 / 653,573 | 653,573 |
| `full_projection.sqlite.dispatch_trace.duration_micros` | 3,843 / 4,460 / 2,293 | 3,843 |
| `synthetic_deep_chain.sqlite.path_max_hops_512.duration_micros` | 553 / 637 / 553 | 553 |

The latest `metric-digest` comparison is sample 3 against sample 2. In the
focused full-projection surface:

- `neighborhood` moved 46,915 -> 47,234 us (+0.68%).
- `path_max_hops` moved 74 -> 68 us (-8.11%).
- `path_max_hops_512` moved 30 -> 16 us (-46.67%).
- `evidence` moved 974 -> 793 us (-18.58%).
- `conflict_matrix` moved 654,459 -> 653,573 us (-0.14%).
- `dispatch_trace` moved 4,460 -> 2,293 us (-48.59%).

## Verdict

`#gbenchneigh` is closed. The performance history now has a fresh,
neighborhood-ranked full-projection sample set where neighborhood ranking,
path-hop tiers, evidence, conflict-matrix, and dispatch-trace can be compared
in the same rows. The fresh evidence still points away from raising the
user-facing 64-hop default based on path probes alone: high-hop probes are
cheap, but the materially larger full-projection comparison costs are still
neighborhood/conflict and projection refresh surfaces.
