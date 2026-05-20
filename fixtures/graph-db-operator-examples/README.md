# Graph DB Operator Examples

These examples document the SQLite and Convex graph DB paths with reusable commands and small fixtures.

## SQLite Graph DB Reads

```bash
tsift graph-db --path . refresh --json
tsift graph-db --path . status --json
tsift graph-db --path . schema --json
tsift graph-db --path . kind backlog --property ref_id=cvxa --limit 5 --json
tsift graph-db --path . evidence cvxa --depth 3 --limit 8 --json
tsift graph-db --path . neighborhood gbak-example --depth 2 --edge-kind mentions --json
```

`graph-db refresh` materializes `.tsift/graph.db` explicitly; `graph-db status`
reports projection version, content hash, source watermark, row counts, and
tombstone counts without refreshing.

## Convex Sync Dry Run

```bash
tsift convex-sync . --snapshot fixtures/graph-db-operator-examples/stale-convex-snapshot.json --chunk-size 25 --json
```

The stale fixture intentionally omits the local projection metadata, so freshness should report a fail-closed plan with node and edge upserts.

## Convex Apply

```bash
TSIFT_CONVEX_GRAPH_URL="https://<deployment>.convex.site/tsift/graph" \
TSIFT_CONVEX_AUTH_TOKEN="<optional bearer token>" \
tsift convex-sync . --remote-snapshot --apply --json
```

Use `examples/convex-graph` for the Convex app-side schema, mutations, and HTTP action that accepts the chunks.

## Live Convex Acceptance

The live acceptance harness is opt-in and should point at a disposable Convex
deployment. It pulls the current remote snapshot, applies the local temporary
projection, pulls the snapshot again, then runs `graph-db` node, kind,
neighborhood, and path parity checks against SQLite and the remote rows:

```bash
TSIFT_LIVE_CONVEX_ACCEPTANCE=1 \
TSIFT_LIVE_CONVEX_GRAPH_URL="https://<deployment>.convex.site/tsift/graph" \
TSIFT_LIVE_CONVEX_AUTH_TOKEN="<optional bearer token>" \
cargo test --test graph_db_conformance \
  live_convex_graph_backend_acceptance_applies_and_matches_graph_db_queries \
  -- --ignored --nocapture
```

## Convex Snapshot Reads

```bash
tsift graph-db \
  --backend convex-snapshot \
  --convex-snapshot /tmp/current-convex-rows.json \
  kind source_handle \
  --limit 10 \
  --json
```

Convex-backed reads fail closed when the supplied snapshot trails `.tsift/graph.db`.

## Handle Reuse

```bash
tsift --envelope context-pack tasks/software/tsift.md --budget normal --json
tsift graph-db --path . evidence '#gref' --depth 3 --limit 8 --json
tsift traverse <source_handle_or_job_packet_handle> --path . --depth 1 --format json
```

The context pack stores `source_handle` and `worker_context` nodes in the graph. Agent-doc queue entries become `job_packet` nodes, so workers can keep handoff scope, source windows, and queued backlog items linked by stable handles.
