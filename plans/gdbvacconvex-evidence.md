# #gdbvacconvex evidence — self-hosted backend workflow proven; demo-schema snapshot has a scale gap

Closes `#gdbvacconvex` for the **workflow-proven, schema-scale-gated** path: the deployment + sync legs of the Convex graph backend are end-to-end verified against the self-hosted `ghcr.io/get-convex/convex-backend:latest` Docker image, with no `convex.dev` account or browser auth involved. The `snapshot` half of the demo schema does not yet scale to the agent-loop projection size; that is captured as a precise follow-up and is the reason `#gdbvacproof` stays review-gated.

## Bypass the auth wall — self-hosted Convex via Docker

The Convex CLI fails closed at an interactive device-name prompt regardless of flags (`bunx convex dev --configure new --once`, `--local`, `--dev-deployment local` — all blocked). The unblocking path is the open-source backend image, which generates its own admin key and never contacts `convex.dev` for account auth:

```bash
# Start backend (binds 3210 for client RPC, 3211 for HTTP actions; uses Docker named volume for persistence).
docker run -d --name convex-tsift --rm \
  -p 3210:3210 -p 3211:3211 \
  -v convex-tsift-data:/convex/data \
  ghcr.io/get-convex/convex-backend:latest

# Generate the admin key inside the container — bypasses convex.dev entirely.
docker exec convex-tsift bash -c "cd /convex && ./generate_admin_key.sh"
#  → convex-self-hosted|01...<64-byte secret>

# Write to examples/convex-graph-app/.env.local (gitignored):
# CONVEX_SELF_HOSTED_URL=http://localhost:3210
# CONVEX_SELF_HOSTED_ADMIN_KEY=<generated key>

# Deploy schema + functions (non-interactive once self-host URL + admin key are set).
cd examples/convex-graph-app && bunx convex dev --once
#  → ✔ Added table indexes: edges.by_edge_key, edges.by_from_kind, edges.by_to_kind,
#                            nodes.by_external_id, nodes.by_kind
#  → ✔ Convex functions ready! (~220 ms)
```

The image runs a beacon ping to `api.convex.dev/api/self_host_beacon` once at startup (anonymous deployment metadata; disable with `--disable-beacon`). No login flow.

## Proven sync workflow — `tsift convex-sync --scope tsift`

Driven against the live HTTP action with the tsift submodule scope (4,767 nodes + 28,427 edges; chunk size 50; 665 chunks total):

```bash
TSIFT_CONVEX_GRAPH_URL="http://localhost:3211/tsift/graph" \
  tsift convex-sync . --scope tsift --remote-snapshot --apply --chunk-size 50 --json \
  > /tmp/gdbvacconvex/sync-tsift-scoped.json
# exit 0
```

| Phase | Result |
| --- | --- |
| `dry_run` | `false` (apply mode) |
| `node_upserts` planned | 4,767 |
| `edge_upserts` planned | 28,427 |
| `node_tombstones` / `edge_tombstones` planned | 0 / 0 (first sync) |
| Chunks dispatched | 665 |
| HTTP receipts (200 OK) | **665 / 665** |
| Transport diagnostics | "live Convex transport completed all planned chunks" |
| Mutation order asserted | "apply node upserts before edge upserts; apply edge tombstones before node tombstones" |
| Freshness verdict | `stale` with `fail_closed=true` (see scale gap below) |

The 665/665 receipt success rate is end-to-end proof that the sync chunk pipeline, the schema-side `upsertNodes` / `upsertEdges` mutations, the foreign-key check inside `upsertEdges` (each edge needs both endpoint nodes already inserted), and the chunk ordering contract all work against a real Convex backend at moderate scale.

## Scale gap — demo `snapshot` query times out

After the sync succeeds, the demo `examples/convex-graph/graph.ts` `snapshot` query still fails on the populated tables:

```bash
curl -sS -X POST http://localhost:3211/tsift/graph \
  -H "content-type: application/json" -d '{"operation":"snapshot"}'
#  → {"code":"Server Error: Uncaught Error: Your request timed out.",
#     "trace":"... at async <anonymous> (../convex/http.ts:21:28) ..."}
```

Root cause: `graph.ts:44-53` calls `ctx.db.query("nodes").collect()` and `ctx.db.query("edges").collect()` in one isolate request. With 33,194 rows the cumulative syscall duration exceeds the Convex isolate's `maximum total syscall duration (maximum duration: 15s)` budget. The same budget applies on the Convex cloud product, so this is a schema-side bug, not a self-hosted limitation.

This is also why `tsift convex-sync`'s `freshness` field came back as `status: stale, snapshot_hash: None` even after a successful apply — the `--remote-snapshot` step couldn't fetch a baseline to diff against because `snapshot` timed out before returning rows.

## What that blocks for `#gdbvacproof`

`tsift graph-db compact --apply --prune-tombstones --confirmed-convex-reconciled` deliberately requires the operator to confirm Convex consumers have reconciled. Without a working `snapshot` round-trip we cannot prove reconciliation, only that writes were accepted (which is necessary but not sufficient). The 781,653 tombstones on the agent-loop superproject `.tsift/graph.db` therefore stay un-pruned, and `#gdbvacproof` stays review-gated.

The local-compact half of `#gdbvacproof` is already evidenced separately at `plans/gdbvacproof-evidence.md` (200.6 MiB / 14,655 freelist pages reclaimed in commit tsift `af7c3ae`).

## Larger-scale sync (out of scope this cycle)

Pushing the full agent-loop superproject graph (357,366 nodes / 604,845 edges) without `--scope` also fails, but earlier in the pipeline — `upsertEdges` itself hits the 99 MiB isolate carry-over limit at chunk-size 100. Smaller chunks help (chunk-size 25 / 50 stayed under the limit on the scoped sync above) but the `snapshot` timeout would still gate `--remote-snapshot`. Both failures are demo-schema problems.

## Follow-ups captured

- **`#convexsnapshotscale`** — Replace `graph.ts:snapshot` with a paginated cursor-based query (or a content-hash-only digest variant) so `tsift convex-sync --remote-snapshot` can verify reconciliation on tables with >5k rows. Without this, `--confirmed-convex-reconciled` for the agent-loop scale graph remains operationally unprovable.
- **`#convexscopedeval`** — Decide whether tsift convex-sync should ship a scope-bounded default that promotes (multi-submodule) reconciliation as the supported large-graph workflow, or whether the bigger workflow waits on snapshot pagination.

## Verdict (closure)

- `#gdbvacconvex` is **closed (workflow-proven, schema-scale-gated)**. The auth-wall blocker is bypassed via self-hosted Docker; the sync write pipeline is end-to-end verified with 665/665 chunk receipts.
- `#gdbvacproof` stays review-gated until `#convexsnapshotscale` lands and `snapshot` returns valid rows on the full agent-loop graph.
