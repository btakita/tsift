# gdbscanproof — graph.db scan-plan evidence

Read-only EXPLAIN + cardinality survey of the SQLite query shapes that back
`tsift graph-db` (and the hot paths inside `src/substrate.rs`) against:

- **real**: a snapshot of the production agent-loop graph at
  `/home/brian/work/btakita/agent-loop/.tsift/graph.db` (copied to
  `/tmp/gdbscanproof/graph.db` so the live writer was untouched).
- **synthetic**: `/tmp/gdbscanproof/synth.db`, a fixture with a single
  high-degree hub (`hub-0` → 10 000 leaves) and a 5 000-node deep call chain.
  Build script: `/tmp/gdbscanproof/build_synth.sql`. The regression tests in
  `tests/scan_plan.rs` rebuild a smaller in-memory variant of the same shape.

Both DBs use the schema and indexes defined in `src/substrate.rs`
(`SqliteGraphStore::ensure_schema` / `ensure_sqlite_graph_edge_properties_schema`).
Sibling backend-eval was idle (no lockfile inside
`.tsift/backend-eval-cache/`), so no run was triggered from this subagent.

## Real-graph cardinalities (snapshot)

| Table | rows |
| --- | --- |
| `graph_nodes` | 3 284 |
| `graph_edges` | 3 464 |
| `graph_node_properties` | 19 617 |
| `graph_edge_properties` | 6 842 |

Top distributions:

- Node kinds: `symbol` 3 197, `file` 40, `source_handle` 23, `backlog` 8.
- Edge kinds: `defines` 3 196, `mentions` 167, `scopes_source` 32,
  `expands_source` 23.
- High-degree hub by `from_id`: `gfil-5481da4ce5` with **1 265** outgoing edges
  (next: 222, 152). Highest `to_id` degree is only 12.
- Edge property `label` has 34 distinct values; the dominant value
  `file defines symbol` covers **3 196** edges (≈92 % of all edges).
- Node property `path` has 41 distinct values; top file
  `src/tsift/src/main.rs` covers **1 266** nodes.

## Synthetic-graph cardinalities

| Table | rows |
| --- | --- |
| `graph_nodes` | 15 001 (1 hub + 10 000 leaves + 5 000 chain nodes) |
| `graph_edges` | 14 999 (10 000 hub→leaf, 4 999 chain) |
| `graph_node_properties` | 10 000 (every leaf gets `path`) |
| `graph_edge_properties` | 10 000 (every hub edge gets `label=hot|cold`) |

## Query-by-query evidence

Plans abbreviated; full output captured below the table.

| Shape | Plan summary (real / synth) | Est rows | Real rows | Scan vs index | Verdict |
| --- | --- | --- | --- | --- | --- |
| `edge_property_scan` (kind+`label=…`) | SEARCH ep0 USING COVERING INDEX `idx_graph_edge_properties_key_value_edge` (key,value); SEARCH e USING `idx_graph_edges_edge_key` | property-prefix limited | 3 196 (real, `label='file defines symbol'`) / 8 000 (synth, `label='cold'`) | index-bound | **acceptable** |
| `incident_edges` (UNION from/to) | CO-ROUTINE MERGE(UNION) of two index probes (`idx_graph_edges_from_kind`, `idx_graph_edges_to_kind`) | per-endpoint degree | 1 271 (real hub `gfil-5481da4ce5`) / 10 000 (synth `hub-0`) | index-bound (top-level `SCAN e` is over the bounded union, not `graph_edges`) | **acceptable** |
| kind + node-property filter | SEARCH `graph_nodes` USING COVERING INDEX `idx_graph_nodes_kind_label`; EXISTS via COVERING `idx_graph_node_properties_key_value_node`; bloom filter at scale | 3 197 nodes pre-filter | 1 265 (real, `path=src/tsift/src/main.rs`) / 7 000 (synth, `path=src/main.rs`) | index-bound | **acceptable** |
| path frontier (single + multi via `IN`) | SEARCH `graph_edges` USING `idx_graph_edges_from_kind` (from_id=?); TEMP B-TREE only for tail `to_id,kind` ordering | per-from_id degree | hub: 1 265 edges (real) / 10 000 (synth) | index-bound | **acceptable** |

### Plan snippets

```
edge_property_scan (real, label='file defines symbol', kind='defines', n≈3196):
  |--SEARCH ep0 USING COVERING INDEX idx_graph_edge_properties_key_value_edge (key=? AND value=?)
  `--SEARCH e   USING INDEX            idx_graph_edges_edge_key             (edge_key=?)
  Wall time: 1.76 ms total (3196 rows joined + counted)

incident_edges (real hub gfil-5481da4ce5, no kind filter):
  |--CO-ROUTINE e
  |  `--MERGE (UNION)
  |     |--LEFT  SEARCH e USING INDEX idx_graph_edges_from_kind (from_id=?)  + TEMP B-TREE FOR ORDER BY
  |     `--RIGHT SEARCH e USING INDEX idx_graph_edges_to_kind   (to_id=?)    + TEMP B-TREE FOR ORDER BY
  `--SCAN e        (over the materialized UNION; bounded by endpoint indexes)
  Wall time: 0.77 ms total (1271 rows)

kind+property (real, kind='symbol', path='src/tsift/src/main.rs'):
  |--SEARCH graph_nodes USING COVERING INDEX idx_graph_nodes_kind_label              (kind=?)
  |--SEARCH p0 EXISTS  USING COVERING INDEX idx_graph_node_properties_key_value_node (key=? AND value=? AND node_id=?)
  `--USE TEMP B-TREE FOR ORDER BY
  Wall time: 0.57 ms (1265 rows)

path frontier (single id, no kind):
  |--SEARCH graph_edges USING INDEX idx_graph_edges_from_kind (from_id=?)
  `--USE TEMP B-TREE FOR LAST 2 TERMS OF ORDER BY
  Wall time: <0.01 ms per probe (hub) on real data; bounded by from_id degree on synth.

path frontier (multi-frontier IN(…), no kind):
  same plan: SEARCH graph_edges USING INDEX idx_graph_edges_from_kind (from_id=?)
  SQLite expands the IN list to per-id probes; no full scan even with 256-id chunk.
```

## Verdicts

- **`edge_property_scan`** → **acceptable**. The plan drives from the
  `(key,value,edge_key)` covering index and joins via the unique
  `idx_graph_edges_edge_key`. Property cardinality is bounded by the leading
  `(key,value)` prefix; the join is a row-at-a-time index seek. No additional
  index is justified — even the worst real value (`label='file defines symbol'`,
  3 196 rows) finishes in under 2 ms end-to-end, well below any
  scan-dominated threshold.
- **`incident_edges`** → **acceptable**. The UNION rewrite (already added in
  `sqlite_incident_edges_union_query`) keeps the two endpoint indexes
  (`idx_graph_edges_from_kind`, `idx_graph_edges_to_kind`) covering the lookup.
  The leaf `SCAN e` in `EXPLAIN` refers to the materialized UNION cursor, not a
  base-table scan; degree-bounded. On a 10 000-degree synthetic hub the full
  count runs in ~1.3 ms.
- **kind + property filters** (`paged_nodes_by_kind` and
  `paged_incident_edges` with `--property KEY=VALUE`) → **acceptable**. SQLite
  uses `idx_graph_nodes_kind_label` to seek by `kind`, then the covering
  property index to satisfy the EXISTS, with the planner adding a bloom filter
  once row counts grow. No fallback to a full `graph_nodes` scan was observed
  on either dataset, even with the synthetic 7 000-row match set.
- **path frontier probes** (`shortest_path_with_max_hops`) → **acceptable**.
  Both single-id and `IN (…)` shapes resolve through
  `idx_graph_edges_from_kind`. The only sort overhead is the `TEMP B-TREE` for
  the trailing `to_id, kind` ordering, which is degree-bounded. The synthetic
  hub probe (10 000 outgoing edges) and the deep-chain probes both stayed
  under 1 ms.

## Recommended index / materialized changes

**None.** Every query shape under audit is already index-bound on both the
production agent-loop graph and the synthetic high-degree + deep-chain
fixture. Adding indexes or materialized rows here would add write-amplification
to refresh without removing any scan that the planner is doing today.

The existing covering indexes that matter are:

- `idx_graph_edges_from_kind (from_id, kind)` — covers single-direction edge
  scans, BFS frontier probes, and the `from_id` half of `incident_edges`.
- `idx_graph_edges_to_kind (to_id, kind)` — covers the `to_id` half of
  `incident_edges` and inbound listings.
- `idx_graph_edges_edge_key UNIQUE (edge_key)` — point-lookup join target for
  the property-driven edge scan.
- `idx_graph_edge_properties_key_value_edge (key, value, edge_key)` —
  covering driver for `edge_property_scan` and edge-side property filters.
- `idx_graph_nodes_kind_label (kind, label, id)` — covering driver for the
  kind+property node scans.
- `idx_graph_node_properties_key_value_node (key, value, node_id)` —
  covering EXISTS target for node-side property filters.

If the surrounding `gdbscanproof` work later proves a scan-dominant shape on
a *different* query (for example bulk export, watermark scans, or a new
join key), revisit this evidence before mutating the schema — none of the
shapes audited here justify it.

## Regression coverage

`tests/scan_plan.rs` builds an in-memory copy of this fixture and asserts
that:

1. `edge_property_scan` uses both `idx_graph_edge_properties_key_value_edge`
   and `idx_graph_edges_edge_key`.
2. The `incident_edges` UNION uses both directional indexes.
3. The kind + property filter uses `idx_graph_nodes_kind_label` and the
   `key_value_node` index.
4. The BFS frontier probe (hub-0 and several chain ids) uses
   `idx_graph_edges_from_kind`.

Each assertion also fails if the plan ever degrades to `SCAN graph_edges` or
`SCAN graph_nodes` against the base tables.
