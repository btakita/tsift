# tsift Convex Graph App (live deployment target)

Standalone Convex app for deploying the tsift graph backend schema, mutations, and HTTP action. Use this as the live target for:

- `tsift convex-sync --remote-snapshot --apply`
- `tsift graph-db compact --prune-tombstones --confirmed-convex-reconciled`
- The `live_convex_graph_backend_acceptance_applies_and_matches_graph_db_queries` ignored test in `tests/graph_db_conformance.rs`

It is the closure path for `#gdbvacconvex` (which itself unblocks the Convex half of `#gdbvacproof`).

The minimal snippet pack lives at `examples/convex-graph/` (schema.ts + graph.ts + http.ts only). This app folder wraps that snippet pack as a deployable Convex project.

## Layout

```
examples/convex-graph-app/
  package.json          # convex + typescript deps
  tsconfig.json
  .gitignore            # node_modules/, .env*, convex/_generated/
  convex/
    schema.ts           # nodes + edges tables and indexes
    graph.ts            # snapshot/upsert/delete query+mutation functions
    http.ts             # POST /tsift/graph router
    _generated/         # emitted by `npx convex dev` (gitignored)
  README.md             # this file
```

## Pre-auth verification (already done by scaffold)

The package layout has been verified before any deployment:

```bash
bun install        # → 6 packages installed (convex 1.39.x, typescript 5.x)
bun run typecheck  # → expected failures: cannot resolve ./_generated/server
                   #   and ./_generated/api. Both directories are emitted by
                   #   `npx convex dev` and are gitignored.
```

The typecheck failure pre-auth is **expected** — `convex/_generated/` only exists after `npx convex dev` runs against a live deployment. Re-run `bun run typecheck` after `convex dev` has finished its initial sync to confirm types resolve.

## One-time deployment (user-action required — interactive auth)

Run from `examples/convex-graph-app/`:

```bash
cd examples/convex-graph-app

# 1. Install deps.
bun install               # or: npm install

# 2. Authenticate + create / select a Convex deployment.
#    First run: opens a browser to log in to convex.dev and pick a dev project.
#    Writes CONVEX_URL into .env.local and emits convex/_generated/.
npx convex dev

#    Leave this running in a separate terminal — `convex dev` watches and
#    redeploys on file changes. When the initial sync prints
#    "Convex functions ready!", the deployment is live.
```

After `convex dev` is running and the initial sync is green:

```bash
# 3. Capture the deployment URL (.env.local is gitignored, do not commit).
cat .env.local
#   → CONVEX_DEPLOYMENT=dev:<slug>
#   → CONVEX_URL=https://<slug>.convex.cloud
#
# The HTTP action address is the .convex.site twin of CONVEX_URL:
#   https://<slug>.convex.site/tsift/graph

# 4. Drive the tsift convex-sync workflow against the live deployment.
TSIFT_CONVEX_GRAPH_URL="https://<slug>.convex.site/tsift/graph" \
  tsift convex-sync . --remote-snapshot --apply --json

# 5. Optional: run the ignored live-acceptance test.
TSIFT_LIVE_CONVEX_ACCEPTANCE=1 \
TSIFT_LIVE_CONVEX_GRAPH_URL="https://<slug>.convex.site/tsift/graph" \
  cargo test --test graph_db_conformance \
    live_convex_graph_backend_acceptance_applies_and_matches_graph_db_queries \
    -- --ignored --nocapture
```

## What this app does not do

- It does NOT auto-deploy in CI. Deployment is a one-time user-side step because `npx convex dev` requires interactive browser auth.
- It does NOT manage production deployments. Use `npx convex deploy --prod` separately when promoting.
- It does NOT manage secrets for you. `.env.local` carries the deployment URL and optional auth token; both are gitignored. If you add `TSIFT_CONVEX_AUTH_TOKEN`, keep it in `.env.local` and never paste it into a shell on a livestream.

## Idempotency contract (mirrors `examples/convex-graph/README.md`)

The HTTP action accepts the operations emitted in `ConvexSyncReport.chunks`:

| Operation | Body shape | Notes |
| --- | --- | --- |
| `snapshot` | `{}` | Returns `{ nodes, edges, indexes }`. |
| `upsert_nodes` | `{ nodeRows: [...] }` | Upsert by `externalId`. |
| `upsert_edges` | `{ edgeRows: [...] }` | Upsert by `edgeKey`; requires both endpoint nodes to exist. |
| `delete_edges` | `{ keys: [...] }` | Idempotent by `edgeKey`. |
| `delete_nodes` | `{ keys: [...] }` | Idempotent by `externalId`. |

Apply order in tsift sync: edge tombstones → node tombstones → node upserts → edge upserts. tsift already emits chunks in that order.

## #gdbvacproof closure path

After this deployment is live, `#gdbvacconvex` can be closed and the Convex half of `#gdbvacproof` becomes executable:

1. Reconcile: `tsift convex-sync . --remote-snapshot --apply --json` and confirm no drift.
2. Local compact dry-run: `tsift graph-db --path . compact --json`. Record file size, freelist, tombstone counts, `status` / `doctor` timings.
3. Local compact apply: `tsift graph-db --path . compact --apply --prune-tombstones --confirmed-convex-reconciled --json`. Record the same metrics post-apply.
4. Capture the before/after deltas + scan-cost evidence into `plans/gdbvacproof-evidence.md` and close `#gdbvacproof`.
