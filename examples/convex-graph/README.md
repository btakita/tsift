# tsift Convex Graph Backend

This example is the app-side contract for `tsift convex-sync --apply`.

Copy `schema.ts`, `graph.ts`, and `http.ts` into a Convex app, or merge the exported table and route definitions into an existing app. Then deploy the Convex app and point tsift at the HTTP action:

```bash
TSIFT_CONVEX_GRAPH_URL="https://<deployment>.convex.site/tsift/graph" \
TSIFT_CONVEX_AUTH_TOKEN="<optional bearer token>" \
tsift convex-sync . --remote-snapshot --apply --json
```

For live acceptance testing, use a dedicated deployment because the harness
reconciles the remote tables to a small temporary projection and tombstones rows
not present in that projection:

```bash
TSIFT_LIVE_CONVEX_ACCEPTANCE=1 \
TSIFT_LIVE_CONVEX_GRAPH_URL="https://<deployment>.convex.site/tsift/graph" \
TSIFT_LIVE_CONVEX_AUTH_TOKEN="<optional bearer token>" \
cargo test --test graph_db_conformance \
  live_convex_graph_backend_acceptance_applies_and_matches_graph_db_queries \
  -- --ignored --nocapture
```

The endpoint accepts the same operations emitted in `ConvexSyncReport.chunks`:

- `snapshot`
- `delete_edges`
- `upsert_nodes`
- `upsert_edges`
- `delete_nodes`

Rows are idempotent by `nodes.externalId` and `edges.edgeKey`. Apply edge tombstones before node tombstones, and node upserts before edge upserts. The tsift CLI already emits chunks in that order.
