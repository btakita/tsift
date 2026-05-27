# gsqlstage - SQLite refresh staging evidence

Goal: close `#gsqlstage` by reducing SQLite graph refresh staging overhead
and keeping the full-projection gate metrics visible for repeated samples:
`sqlite_delta_write`, `sqlite_edge_staging`, `post_write_reads`, and
`duration_micros_per_1k_graph_rows`.

## Implementation

`SqliteGraphStore::replace_projection_with_version` now bulk-stages graph node
and edge rows into the refresh temp tables with bounded multi-row inserts.
Rows are loaded in chunks of 50 before SQL-side row-hash delta comparison.

The existing refresh path continues to:

- stage provider-neutral graph rows in temp tables before merging into
  `graph_nodes` and `graph_edges`
- expand materialized node/edge property rows only for new or row-hash-changed
  owners
- skip unchanged owner property deletes/upserts and count reused materialized
  property rows as unchanged
- report the SQLite write sub-phases through backend-eval metrics

## Coverage

`sqlite_projection_refresh_handles_bulk_row_replacement` now asserts that the
second refresh uses the bulk staging phase details for both node and edge temp
tables, while still proving unchanged row owners skip property staging:

- `sqlite_node_staging` reports bulk staging of 126 graph node rows
- `sqlite_edge_staging` reports bulk staging of 124 graph edge rows
- `refresh.upserted_properties == 0`
- `temp.next_graph_node_properties` staged row count is `0`
- `temp.next_graph_edge_properties` staged row count is `0`

## Fresh samples

Commands ran from `/home/brian/work/btakita/agent-loop/src/tsift` after
`cargo install --path .`:

```bash
tsift graph-db --json backend-eval --full-projection > target/perf/gsqlstage-full-projection-1.json
tsift graph-db --json backend-eval --full-projection > target/perf/gsqlstage-full-projection-2.json
tsift graph-db --json backend-eval --full-projection > target/perf/gsqlstage-full-projection-3.json
```

| Sample | cache hit | graph rows | `sqlite_node_staging` (us) | `sqlite_edge_staging` (us) | `sqlite_delta_write` (us) | `post_write_reads` (us) | `total_duration_micros_per_1k_graph_rows` |
| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| 1 | 0 | 27,742 | 11,532 | 91,698 | 164,157 | 52,152 | 14,186.648 |
| 2 | 1 | 27,742 | 10,810 | 87,936 | 168,047 | 51,666 | 14,093.829 |
| 3 | 1 | 27,742 | 10,004 | 83,330 | 171,127 | 48,986 | 13,918.030 |

Sample 2 phase details:

- `full_projection.sqlite.sqlite_node_staging`: `bulk stage 4147 graph_nodes rows into temp table using multi-row chunks up to 50 rows before delta comparison`
- `full_projection.sqlite.sqlite_edge_staging`: `bulk stage 23595 graph_edges rows into temp table using multi-row chunks up to 50 rows before delta comparison`

## Verification

- `cargo test -q sqlite_projection_refresh_handles_bulk_row_replacement`
- `cargo build`
- `cargo install --path .`
- `make check`

## Verdict

`#gsqlstage` is closed. SQLite projection refresh now bulk-stages graph
node/edge rows through bounded temp-table chunks, keeps unchanged materialized
property owners out of the write path, and reports the requested backend-eval
gate metrics across repeated full-projection samples.
