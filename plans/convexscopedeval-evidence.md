# #convexscopedeval evidence — full-graph default, `--scope` as escape hatch

Closes review item `#convexscopedeval`. Decision: **default to full-graph sync** now that `#convexsnapshotscale` (v0.1.56) shipped cursor pagination for `--remote-snapshot`. Scope-bounded sync stays valid as an explicit recovery / partial-target option.

## Context (what changed since the question was framed)

When `#convexscopedeval` was captured in `plans/gdbvacconvex-evidence.md` the demo schema's `snapshot` query was the binding scale gate (15s syscall budget on `.collect()` over populated tables, plus a 8192-row JSON array cap). At that point a scope-bounded default would have been the conservative choice because it was the only workflow that could complete `--remote-snapshot` end-to-end.

`#convexsnapshotscale` (v0.1.56, tsift `102a1d2`) replaced that with `snapshotMeta` + `snapshotNodesPage` + `snapshotEdgesPage` cursor pagination. Live test against self-hosted Convex confirmed the full-graph snapshot now returns `freshness.status: current` with `snapshot_hash == local_hash` at the 26k+ row scale (3,895 nodes / 22,342 edges across 45 + 8 pages).

That removes the scale rationale for scoped-default. The two remaining considerations are operational, not technical.

## Decision matrix

| Concern | Full-graph default | Scope-bounded default |
| --- | --- | --- |
| Snapshot scale (after v0.1.56) | ✅ paginated transport handles it | ✅ inherently bounded |
| `upsertEdges` isolate carry-over (99 MiB) at chunk 100 | ⚠️ needs `--chunk-size 50` for large applies | ✅ smaller graphs naturally fit |
| Mental model | ✅ "one graph, one Convex deployment" | ⚠️ requires operators to think per-submodule |
| Cross-submodule queries on Convex side | ✅ single deployment | ❌ requires manual federation |
| Failure isolation | ⚠️ one chunk failure surfaces against the whole graph | ✅ one scope's failure doesn't block others |
| Partial-failure recovery | ⚠️ replays whole graph by default; needs targeting | ✅ replay-one-scope is natural |
| Matches existing tsift workspace mental model | ⚠️ flatter | ✅ mirrors per-submodule indexing |

Three of the seven rows favor scope-bounded; four favor full-graph. The deciding factor is the second consideration after scale: **operational reach**. Full-graph default means one configured `TSIFT_CONVEX_GRAPH_URL` reconciles the whole project. Scoped default would require multiple Convex deployments or a discriminator-by-scope schema, neither of which is shipped today.

## Verdict

- **Default**: full-graph (`tsift convex-sync .` with no `--scope`).
- **Escape hatch (`--scope <name>`)** is the supported workflow for:
  1. Reconciling one submodule independently while the rest of the graph is mid-flight.
  2. Very-large projects where a single Convex deployment is not the operational target.
  3. Partial-failure recovery where only one scope's chunks need replay.
- **Default `--chunk-size` lowered from 100 to 50** to keep `upsertEdges` under the Convex isolate's 99 MiB carry-over budget on the demo schema. Operators targeting an optimized schema can raise it back. This is the operational consequence of choosing full-graph default: applies cover more rows per run, so the per-chunk budget needs to be tighter by default.

## Code changes

- `src/main.rs:401` — `chunk_size` clap arg `default_value` `"100"` → `"50"`; help string explains the rationale.
- `SPEC.md` — `tsift convex-sync` paragraph now states the full-graph-default contract, the `--chunk-size 50` rationale, and lists the three escape-hatch cases.
- `VERSIONS.md` — `## 0.1.57` entry summarizing the decision.

## Verification

- `cargo check --tests` clean.
- `cargo test cmd_convex_sync` — existing test passes (uses `chunk_size: 100` directly, decoupled from the CLI default).
- `make check` exit 0.
- `cargo install --path .` — `~/.cargo/bin/tsift` upgraded to 0.1.57.

## Follow-ups (out of scope this cycle)

- A separate ticket should evaluate whether the demo `upsertEdges` mutation itself can be refactored to lift the 99 MiB carry-over so the default `--chunk-size` can return to 100. The current mutation does sequential `await ctx.db.query("nodes").withIndex(...).unique()` for each edge endpoint; batching those lookups would let larger chunks complete inside one isolate invocation. Captured here for future decision, not opened as a backlog item.
- `complete [#gdbvacproof]` (queue head after this) can now proceed end-to-end: full-graph reconcile via paginated `--remote-snapshot`, `--apply` with the new safer chunk default, then `tsift graph-db --path . compact --apply --prune-tombstones --confirmed-convex-reconciled --json` with metric capture.

## Verdict

`#convexscopedeval` resolved: full-graph default, `--scope` escape hatch documented, `--chunk-size` default tightened. Decision is reversible — flip the default back to 100 once the demo schema's upsert mutations are batched, or push the scope-default if cross-deployment becomes the canonical workflow.
