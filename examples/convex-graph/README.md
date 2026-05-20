# tsift Convex Graph Backend

This example is the app-side contract for `tsift convex-sync --apply`.

Copy `schema.ts`, `graph.ts`, and `http.ts` into a Convex app, or merge the exported table and route definitions into an existing app. Then deploy the Convex app and point tsift at the HTTP action:

```bash
TSIFT_CONVEX_GRAPH_URL="https://<deployment>.convex.site/tsift/graph" \
TSIFT_CONVEX_AUTH_TOKEN="<optional bearer token>" \
tsift convex-sync . --remote-snapshot --apply --json
```

The endpoint accepts the same operations emitted in `ConvexSyncReport.chunks`:

- `snapshot`
- `delete_edges`
- `upsert_nodes`
- `upsert_edges`
- `delete_nodes`

Rows are idempotent by `nodes.externalId` and `edges.edgeKey`. Apply edge tombstones before node tombstones, and node upserts before edge upserts. The tsift CLI already emits chunks in that order.

