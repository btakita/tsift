# gsqlwrite - SQLite refresh write and staging evidence

Goal: close `#gsqlwrite` by reducing repeated SQLite projection refresh
property-row staging and by making the full-projection write hotspots binding
backend-eval gate metrics.

## Implementation

`SqliteGraphStore::replace_projection_with_version` now derives temp
`next_graph_changed_nodes` and `next_graph_changed_edges` owner sets before
materializing property rows. Normal refreshes include only new or row-hash
changed owners in those sets; forced refreshes still include every owner.

Materialized property staging, property deletes, and property upserts now operate
only on changed owners. Existing materialized property rows are counted as reused
for unchanged row-hash owners, so repeated refreshes no longer expand JSON or
touch property rows for owners whose persisted row hash already matches the
incoming projection.

Backend-eval now exposes direct metrics for full-projection SQLite write
sub-phases and requires the queue-requested keys in the full-projection
performance gate:

- `full_projection.sqlite.sqlite_delta_write.duration_micros`
- `full_projection.sqlite.sqlite_edge_staging.duration_micros`
- `full_projection.sqlite.post_write_reads.duration_micros`
- `full_projection.sqlite.total_duration_micros_per_1k_graph_rows`

## Coverage

`sqlite_projection_refresh_handles_bulk_row_replacement` now performs a second
refresh where all surviving owners are unchanged and asserts:

- `refresh.unchanged_nodes == 126`
- `refresh.unchanged_edges == 124`
- `refresh.upserted_properties == 0`
- `temp.next_graph_node_properties` staged row count is `0`
- `temp.next_graph_edge_properties` staged row count is `0`
- node and edge property-row staging phase details state that only new/changed
  owner rows are expanded

`graph_db_backend_eval_benchmarks_candidate_stores_against_sqlite` now asserts
that the new full-projection SQLite phase metrics are present in both
`metrics` and `performance_gate.required_metrics`.

## Fresh samples

Commands ran from `/home/brian/work/btakita/agent-loop/src/tsift` after
`cargo install --path .`:

```bash
tsift graph-db --json backend-eval --full-projection > target/perf/gsqlwrite-warm.json
tsift graph-db --json backend-eval --full-projection > target/perf/gsqlwrite-cache-1.json
tsift graph-db --json backend-eval --full-projection > target/perf/gsqlwrite-cache-2.json
tsift graph-db --json backend-eval --full-projection > target/perf/gsqlwrite-cache-3.json
```

| Sample | cache hit | `sqlite_delta_write` (us) | `sqlite_edge_staging` (us) | `post_write_reads` (us) | `total_duration_micros_per_1k_graph_rows` |
| --- | ---: | ---: | ---: | ---: | ---: |
| warm | 0 | 154,235 | 65,155 | 54,524 | 13,082.916 |
| cache-1 | 1 | 146,361 | 58,267 | 50,322 | 12,211.481 |
| cache-2 | 1 | 148,544 | 57,270 | 49,824 | 12,202.692 |
| cache-3 | 1 | 147,382 | 57,348 | 51,085 | 12,201.357 |

These samples prove the gate keys are emitted on the installed binary. They do
not prove the changed-owner property staging optimization on the full-projection
path because backend-eval intentionally writes the cached provider-neutral rows
into a fresh in-memory SQLite store for each full-projection sample, so every
owner is new to that store. The persistent-refresh win is covered by the direct
refresh test above.

## Verification

- `cargo test -q sqlite_projection_refresh_handles_bulk_row_replacement`
- `cargo test -q graph_db_backend_eval_benchmarks_candidate_stores_against_sqlite`
- `cargo build`
- `cargo install --path .`
- `make check`

## Verdict

`#gsqlwrite` is closed. Repeated SQLite projection refreshes skip unchanged
owner property staging and rewrites, and full-projection backend-eval now gates
the requested SQLite write sub-phases.
