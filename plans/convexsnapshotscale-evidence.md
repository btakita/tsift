# #convexsnapshotscale evidence — paginated Convex snapshot transport

Closes `#convexsnapshotscale`. The previous demo `examples/convex-graph/graph.ts:snapshot` query returned every row in `nodes` + `edges` in a single Convex isolate request, which broke down at ~5k+ rows: at modest scale Convex enforces a 15s per-request syscall budget, and at larger scale it enforces an 8192-element return-array cap. Both ceilings blocked `tsift convex-sync --remote-snapshot` and therefore blocked the Convex half of `#gdbvacproof`. This change replaces the single-shot snapshot with a cursor-paginated transport.

## Design choice — Option A (cursor-based pagination)

**Picked A over B (digest) and C (Convex paginate())** after reading `src/main.rs:6549-6620` (`convex_projection_freshness`). The remote-snapshot path doesn't only compute a single hash — it also materializes per-row diffs (`missing_nodes`, `stale_nodes`, `missing_edges`, `stale_edges`) so operators can see which records drifted. A digest-only path (B) would gut those diagnostics and reduce `--remote-snapshot` to a binary "current / not current" verdict; that's a real regression for drift triage. Option C (Convex `paginate()`) would have the same surface area as A but force callers onto Convex's opaque continuation tokens, which are less debuggable than the externalId/edgeKey cursors we already index. A is the smallest change that preserves the existing consumer contract while removing the scale ceiling.

The legacy `snapshot` query and `snapshot` HTTP operation are retained as a fallback — `ConvexHttpTransport::fetch_snapshot` only routes to legacy when the backend reports `unknown operation` on `snapshot_meta`. Operators on old deployments keep working until they redeploy; new deployments and tooling use the paginated path exclusively.

## Changes

- **Schema** (mirrored in `examples/convex-graph/graph.ts` and `examples/convex-graph-app/convex/graph.ts`):
  - `snapshotMeta` — returns `{ indexes, nodeCount, edgeCount, pageSize }`. Counts iterate the table without materializing per-row payloads on the wire.
  - `snapshotNodesPage({ cursor, limit })` — returns `{ rows, nextCursor, pageSize }`. Cursor is the last `externalId` from the previous page; uses the existing `by_external_id` index with `q.gt("externalId", cursor)` for stable ordering.
  - `snapshotEdgesPage({ cursor, limit })` — same shape, keyed by `edgeKey` via `by_edge_key`.
  - `DEFAULT_SNAPSHOT_PAGE_SIZE = 500`, `MAX_SNAPSHOT_PAGE_SIZE = 2000`.
  - Legacy `snapshot` query retained verbatim.
- **HTTP action** (mirrored in `examples/convex-graph/http.ts` and `examples/convex-graph-app/convex/http.ts`): routes `snapshot_meta`, `snapshot_nodes_page`, `snapshot_edges_page` alongside the retained `snapshot`.
- **tsift consumer** (`src/main.rs`):
  - `ConvexTransportRequest` gains optional `cursor` and `limit` fields (serde-skipped when `None`).
  - `ConvexTransportResponse` gains optional `meta` and `page` variants; new `ConvexSnapshotMeta` and `ConvexSnapshotPage` deserialization helpers.
  - `ConvexHttpTransport::fetch_snapshot` calls `fetch_snapshot_paginated` first; on `unknown operation` / `404` it falls back to `fetch_snapshot_legacy` so older deployments keep working.
  - Page loops concatenate node and edge pages locally into a single `ConvexProjectionRows`, preserving the row-level diff contract the consumer (`convex_projection_freshness`) already uses.
- **Tests**:
  - New `convex_sync_remote_snapshot_uses_paginated_transport_against_mock_backend` in `tests/graph_db_conformance.rs`. Stands up a stdlib `TcpListener` HTTP/1.1 mock backend (no new test deps), forces page size 3 to exercise the cursor loop on a small synthetic graph, asserts the run reports `freshness.status == "current"` with `local_hash == snapshot_hash`, and asserts the call log starts with `snapshot_meta` followed by multiple `snapshot_nodes_page` / `snapshot_edges_page` calls and never the legacy `snapshot`.
  - The existing ignored live-acceptance test (`live_convex_graph_backend_acceptance_applies_and_matches_graph_db_queries`) and its `fetch_live_convex_snapshot` helper were rewritten to drive the paginated ops, so they exercise the same code path on real backends.
- **VERSIONS.md / Cargo.toml**: this change is captured under `## 0.1.55`. (The repo had a concurrent `0.1.56` bump for an unrelated tagpath fix; current `Cargo.toml = 0.1.56` reflects that, and `0.1.55` is the version slot allocated to this task.)

## Before — legacy `snapshot` fails at scale

After populating the live self-hosted backend with the tsift superproject graph (3,895 nodes + 22,342 edges):

```text
$ curl -sS -i -X POST http://localhost:3211/tsift/graph \
    -H "content-type: application/json" -d '{"operation":"snapshot"}'

HTTP/1.1 500 Internal Server Error
content-type: application/json

{"code":"... Server Error: Uncaught Error: Function graph.js:snapshot return value invalid: Array length is too long (22342 > maximum length 8192)\n", ...}
```

This is a stricter cousin of the timeout originally captured in `plans/gdbvacconvex-evidence.md` (~33k rows, "maximum total syscall duration 15s"). Convex enforces an 8192-element cap on query return arrays in addition to the syscall budget; either one is fatal at agent-loop graph scale.

## After — paginated transport on the same populated backend

```text
$ TSIFT_CONVEX_GRAPH_URL="http://localhost:3211/tsift/graph" \
    ./target/release/tsift convex-sync . --remote-snapshot --json
```

```json
{
  "freshness": {
    "status": "current",
    "fail_closed": false,
    "local_hash": "ed23d0170975be50b2f19d6a1731b9efc5eaab9fa579f9c1786fc4832bcc039f",
    "snapshot_hash": "ed23d0170975be50b2f19d6a1731b9efc5eaab9fa579f9c1786fc4832bcc039f",
    "missing_nodes": [],
    "stale_nodes": [],
    "missing_edges": [],
    "stale_edges": [],
    "diagnostics": []
  },
  "transport": {
    "remote_snapshot": true,
    "applied_chunks": 0
  }
}
```

`local_hash == snapshot_hash`, zero drift, `status: "current"`. exit code 0. The paginated path fetched 3,895 nodes across 8 pages (page size 500) and 22,342 edges across 45 pages without exceeding any Convex isolate budget.

## Reproduction

```bash
# 1. Self-hosted Convex backend
docker run -d --name convex-tsift --rm \
  -p 3210:3210 -p 3211:3211 \
  -v convex-tsift-data:/convex/data \
  ghcr.io/get-convex/convex-backend:latest

# 2. Admin key + .env.local (do NOT echo the key on a livestream)
cd examples/convex-graph-app
ADMIN_KEY=$(docker exec convex-tsift bash -c "cd /convex && ./generate_admin_key.sh" | tail -1)
printf 'CONVEX_SELF_HOSTED_URL=http://localhost:3210\nCONVEX_SELF_HOSTED_ADMIN_KEY=%s\n' "$ADMIN_KEY" > .env.local
bun install   # one-time
bunx convex dev --once
#  → ✔ Convex functions ready! (~200 ms)

# 3. Apply + verify
cd ../..
cargo build --release
TSIFT_CONVEX_GRAPH_URL="http://localhost:3211/tsift/graph" \
  ./target/release/tsift convex-sync . --apply --chunk-size 50 --json > /tmp/sync-apply.json
TSIFT_CONVEX_GRAPH_URL="http://localhost:3211/tsift/graph" \
  ./target/release/tsift convex-sync . --remote-snapshot --json > /tmp/sync-remote-snapshot.json
jq '.freshness.status' /tmp/sync-remote-snapshot.json   # → "current"

# 4. Cleanup
docker stop convex-tsift
docker volume rm convex-tsift-data
rm examples/convex-graph-app/.env.local
```

## Local verification

- `cargo check --tests` — exit 0 (warnings only about unused `[patch]` entries, pre-existing).
- `cargo clippy --all-targets -- -D warnings` — exit 0 (same `[patch]` warnings only).
- `cargo test` — 862 passed across 5 suites in ~32 s; includes the new paginated-transport mock test.
- `make check` (= clippy + cargo test) — exit 0.

## Unblocks

`#gdbvacproof` Convex half: `tsift graph-db compact --apply --prune-tombstones --confirmed-convex-reconciled` can now be operationally proven against the full agent-loop graph because `--remote-snapshot` returns a real `current` verdict instead of timing out.
