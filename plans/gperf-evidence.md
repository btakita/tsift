# gperf - graph-db performance triage

Goal: refresh `graph-db backend-eval --full-projection` evidence after the
cache and tombstone-prune work, covering the real, full-projection,
high-degree, and deep-chain workloads before changing hop caps or backend
promotion policy.

## Commands

All samples used the installed `tsift 0.1.62` binary from the agent-loop
superproject:

```bash
tsift graph-db --path /home/brian/work/btakita/agent-loop --json backend-eval --full-projection
```

Raw reports:

- `target/perf/gperf-sample-1.json`
- `target/perf/gperf-sample-2.json`
- `target/perf/gperf-sample-3.json`
- `target/perf/gperf-metric-digest.json`

One control run written outside the repository (`/tmp/tsift-gperf-control.json`)
also missed the full-projection cache, so the repeated miss behavior is not an
artifact of writing reports under `target/`.

## Three-sample summary

| Metric | Samples (us) | Median (us) |
| --- | ---: | ---: |
| `full_projection.cache.hit` | 0, 0, 0 | 0 |
| `real.sqlite.total_duration_micros` | 53,556,009 / 39,551,515 / 610,342 | 39,551,515 |
| `full_projection.sqlite.total_duration_micros` | 22,064,969 / 21,996,993 / 22,304,717 | 22,064,969 |
| `synthetic_high_degree.sqlite.total_duration_micros` | 29,904 / 35,103 / 8,466 | 29,904 |
| `synthetic_deep_chain.sqlite.total_duration_micros` | 12,767 / 12,293 / 10,071 | 12,293 |
| `full_projection.sqlite.conflict_matrix.duration_micros` | 662,428 / 667,605 / 680,052 | 667,605 |
| `real.sqlite.conflict_matrix.duration_micros` | 590,454 / 582,598 / 597,838 | 590,454 |
| `synthetic_deep_chain.sqlite.path_max_hops_512.duration_micros` | 566 / 575 / 514 | 566 |

## Hotspot ranking

The largest current costs are still projection construction and SQLite write
work, not the high-hop path probes:

| Phase | Samples (us) | Median (us) |
| --- | ---: | ---: |
| `full_projection.source_graph_build` | 24,344,246 / 24,103,353 / 23,272,882 | 24,103,353 |
| `full_projection.sqlite.replace_projection_total` | 18,703,736 / 18,634,384 / 18,783,303 | 18,703,736 |
| `full_projection.sqlite.sqlite_delta_write` | 11,153,707 / 11,117,368 / 11,368,293 | 11,153,707 |
| `full_projection.sqlite.sqlite_edge_staging` | 3,377,007 / 3,402,843 / 3,391,323 | 3,391,323 |
| `full_projection.projection_rows` | 3,156,228 / 3,175,680 / 2,895,852 | 3,156,228 |
| `full_projection.sqlite.post_write_reads` | 2,654,603 / 2,651,479 / 2,789,092 | 2,654,603 |

`metric-digest --baseline fixtures/graph-db-performance-history.json` ranked
the latest run's top regression as
`full_projection.refresh_phase.source_graph_build.duration_micros`
(22,746,431 -> 23,272,882 us, +2.31%). The top synthetic path-tier values were
sub-millisecond (`deep_chain.path_max_hops_512` median 566 us), so the current
evidence does not justify backend or hop-cap changes.

## Fixture update

Three new runs were appended to
`fixtures/graph-db-performance-history.json`:

- `agent-loop-gperf-full-projection-2026-05-26-sample-1`
- `agent-loop-gperf-full-projection-2026-05-26-sample-2`
- `agent-loop-gperf-full-projection-2026-05-26-sample-3`

Each run carries all four required workload prefixes through its metric map and
records `cache_state="miss"` because all full-projection cache checks missed
and pruned one sibling cache file.

## Verdict

Do not raise user-facing `max-hop` defaults from 64 yet, and do not promote a
prototype backend based on this evidence. The next performance work should
target source-watermark/cache stability and full-projection projection/write
costs before revisiting caps or adapter promotion.
