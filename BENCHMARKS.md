# tsift Graph DB Benchmarks

Benchmark results from `tsift graph-db backend-eval`. All times are median across repeated cache-hit samples unless otherwise noted.

## SQLite vs SurrealDB (2026-06-02)

Three `backend-eval --candidate surrealdb --full-projection` cache-hit samples. SurrealDB uses Rust-side derived indexes and batch projection load.

### Aggregate Workloads

| Workload | SQLite | SurrealDB | Ratio |
|---|---|---|---|
| Real (session-scoped) | 2.6 ms | 573.2 ms | SurrealDB ~220x slower |
| Full-projection (~952k rows) | 544.2 ms | 535.0 ms | Roughly competitive |
| High-degree (synthetic) | 11.0 ms | 7.4 ms | SurrealDB 1.5x faster |
| Deep-chain (synthetic) | 12.0 ms | 4.3 ms | SurrealDB ~3x faster |

### Refresh / Projection Load

| Workload | SQLite | SurrealDB | Ratio |
|---|---|---|---|
| Full-projection refresh/load | 541.8 ms | 82.7 ms | SurrealDB 6.5x faster |
| Real refresh/load | 0.18 ms | 88.6 ms | SurrealDB 492x slower |

### Promotion Verdict: Hold

SurrealDB remains blocked on the real workload. SQLite 2.6 ms vs SurrealDB 573.2 ms — the per-store tokio runtime + SurrealKV open cost dominates at small graph sizes. Operation-level gates (edge lookup, edge-property scan, incident edges, neighborhood, path tiers) all regress on the real workload.

Bright spots: SurrealDB batch projection load wins on full-projection (82.7 ms vs 541.8 ms) and outperforms SQLite on synthetic high-degree and deep-chain workloads.

## Multi-Backend Comparison (2026-05-24 fixture)

From `fixtures/graph-db-performance-history.json` — single full-projection + real run on agent-loop (355k nodes, 596k edges, ~952k graph rows). All candidates are read-only prototypes evaluated through the same `GraphStore` trait.

### Full-Projection Total Duration (µs)

| Backend | Sample 1 | Sample 2 |
|---|---|---|
| SQLite | 22,960,403 | 19,135,075 |
| DuckDB/DuckPGQ | 15,402,543 | 15,052,299 |
| FalkorDB | 15,709,845 | 15,424,846 |
| Kuzu | 15,508,087 | 15,385,474 |
| Ladybug | 15,079,986 | 15,345,480 |

SQLite total is refresh-dominated (~22M µs refresh). Excluding refresh, SQLite hot-read operations are sub-millisecond.

### Real Workload Total Duration (µs)

| Backend | Sample 1 | Sample 2 |
|---|---|---|
| SQLite | 37,732,560 | 32,769,403 |
| DuckDB/DuckPGQ | 21,511,139 | 18,494,546 |
| FalkorDB | 23,178,417 | 18,898,289 |
| Kuzu | 16,588,261 | 14,456,954 |
| Ladybug | 22,063,074 | 16,622,015 |

### Full-Projection Per-Operation (Sample 1, SQLite vs candidates, µs)

| Operation | SQLite | DuckDB | FalkorDB | Kuzu | Ladybug |
|---|---|---|---|---|---|
| edge_lookup | 15,767 | 2,414,900 | 2,233,795 | 2,288,878 | 2,318,256 |
| incident_edges | 343 | 2,481,528 | 2,587,743 | 2,335,448 | 2,236,432 |
| edge_property_scan | 19,057 | 1,403,150 | 1,597,519 | 1,288,380 | 1,251,829 |
| path_max_hops | 65 | 1,026,613 | 1,047,352 | 1,146,153 | 1,063,267 |
| conflict_matrix | 817,249 | 1,691,484 | 1,785,759 | 1,879,996 | 1,798,979 |
| refresh | 22,098,441 | 3,378,403 | 3,026,385 | 3,162,001 | 2,815,200 |

SQLite dominates per-operation hot reads (sub-ms on cached data). Candidate backends show faster refresh/projection load but orders-of-magnitude slower hot reads via their external query interfaces.

### Synthetic Deep-Chain (1,280 rows)

| Backend | Total (µs) |
|---|---|
| SQLite | 15,494 |
| DuckDB/DuckPGQ | 13,790 |
| FalkorDB | 10,150 |
| Kuzu | 9,596 |
| Ladybug | 9,034 |

### Synthetic High-Degree (1,089 rows)

| Backend | Total (µs) |
|---|---|
| SQLite | 34,966 |
| DuckDB/DuckPGQ | 13,619 |
| FalkorDB | 9,576 |
| Kuzu | 8,410 |
| Ladybug | 9,643 |

## Fixture Source

Raw metrics: `fixtures/graph-db-performance-history.json`

Run command:
```bash
tsift graph-db --path . --json backend-eval \
  --candidate duckdb-duckpgq --candidate falkordb \
  --candidate ladybug --candidate kuzu --candidate surrealdb \
  --full-projection --target <scope>
```
