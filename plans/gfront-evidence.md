# #gfront Evidence

Date: 2026-05-26

Prompt: optimize SQLite high-hop/frontier traversal if refreshed metrics show path/evidence latency is still hot; otherwise keep the 64-hop cap guarded by metrics and query-plan tests.

## Refreshed Sample

Command:

```bash
tsift graph-db --path /home/brian/work/btakita/agent-loop --json backend-eval --full-projection | jq '{metrics: (.metrics | with_entries(select(.key | test("path_max_hops|evidence")))), notes: .notes}'
```

The run completed after the known Kotlin grammar warnings for two JetBrains files. SQLite path/evidence metrics were not hot relative to source graph build and projection-write work:

| Workload | SQLite evidence_target_resolution | SQLite evidence | SQLite path 64 | SQLite path 128 | SQLite path 256 | SQLite path 512 |
| --- | ---: | ---: | ---: | ---: | ---: | ---: |
| real | 616 us | 1106 us | 55 us | 30 us | 26 us | 26 us |
| full_projection | 515 us | 957 us | 45 us | 28 us | 31 us | 26 us |
| synthetic_high_degree | 54 us | 300 us | 35 us | 17 us | 13 us | 12 us |
| synthetic_deep_chain | 50 us | 242 us | 90 us | 146 us | 281 us | 564 us |

The prototype read-only stores still spend roughly 1.1s on real/full-projection `path_max_hops` probes, but that is not SQLite frontier code and belongs to the backend-adapter spike (`#gback`).

## Decision

Do not rewrite SQLite frontier traversal in this cycle. The existing SQLite implementation already uses indexed frontier expansion through `idx_graph_edges_from_kind`, and the refreshed metrics do not justify replacing it with a recursive CTE or another traversal path.

Instead, this cycle tightened the guard:

- Full-projection performance gate now requires SQLite `evidence_target_resolution`, `evidence`, and 64/128/256/512-hop path duration metrics.
- Scan-plan coverage now proves chunked frontier probes keep using `idx_graph_edges_from_kind`, extending the existing single-node frontier plan test.
- SPEC now records that SQLite frontier rewrites require refreshed hot metrics, not candidate-backend path regressions.
