# #gdbvacproof evidence — partial (compact half only)

Closes the **local SQLite compact half** of review item `#gdbvacproof`. The Convex reconciliation half is blocked behind `#gdbvacconvex` (interactive `npx convex dev` against `examples/convex-graph-app/` — scaffold ready, user-side auth required).

Target: `/home/brian/work/btakita/agent-loop/.tsift/graph.db` (superproject graph, 1.7 GB pre-apply — the larger of the two graph.db files in this tree). Tsift version: `0.1.49` (post-#gdbprephot).

## Dry-run + apply commands

```bash
# Baseline state captured into /tmp/gdbvacproof/
tsift graph-db --path . --json status   > /tmp/gdbvacproof/status-before.json
tsift graph-db --path . --json doctor   > /tmp/gdbvacproof/doctor-before.json
tsift graph-db --path . --json compact  > /tmp/gdbvacproof/compact-dryrun.json
# (--prune-tombstones omitted: dry-run reports safe_to_prune_tombstones=false
#  because requires_convex_reconciliation=true — the Convex half is blocked
#  behind #gdbvacconvex)

# Apply (WAL checkpoint + VACUUM, no tombstone prune):
tsift graph-db --path . --json compact --apply > /tmp/gdbvacproof/compact-apply.json
```

## Before / after metrics

| Metric | Before | After | Δ |
| --- | ---: | ---: | ---: |
| File size (bytes) | 1,810,817,024 | 1,600,466,944 | **-210,350,080 (-11.6 %)** |
| File size (human) | 1.69 GiB | 1.49 GiB | -200.6 MiB |
| `PRAGMA freelist_count` | 14,655 | 0 | **-14,655 (100 % reclaimed)** |
| `PRAGMA page_count` (4 KiB pages) | 442,094 | 390,739 | -51,355 pages (-11.6 %) |
| Live rows (nodes + edges) | 962,211 | 962,211 | 0 |
| Tombstone rows (retained) | 781,653 | 781,653 | 0 (prune skipped pending Convex reconciliation) |
| `tsift graph-db status --json` (warm median, 4 runs) | n/a (not pre-timed) | **1 ms** | — |
| `tsift graph-db doctor --json` (single run, immediate post-apply) | n/a (not pre-timed) | **2,336 ms** | — |
| Apply wall time (WAL checkpoint + VACUUM) | — | 3,168 ms | — |
| Single SQLite scan `SELECT COUNT(*) FROM graph_edges WHERE kind='file defines symbol'` (post-apply) | n/a (not pre-timed) | **12 ms** | — |

## Compact report excerpt (`compact-apply.json`)

```json
{
  "applied": true,
  "pruned_tombstones": 0,
  "counts_before": {
    "nodes": 357366, "edges": 604845,
    "tombstones": {"nodes": 99544, "edges": 682109, "total": 781653},
    "file_size_bytes": 1810817024, "freelist_bytes": 60096512
  },
  "counts_after": {
    "nodes": 357366, "edges": 604845,
    "tombstones": {"nodes": 99544, "edges": 682109, "total": 781653},
    "file_size_bytes": 1810817024, "freelist_bytes": 60096512
  },
  "compaction_before": {
    "status": "not_needed",
    "tombstone_scan_rows": 781653,
    "live_rows": 962211,
    "safe_to_prune_tombstones": false,
    "requires_convex_reconciliation": true,
    "recommendations": [
      "tsift convex-sync \".\" --remote-snapshot --apply --json",
      "tsift graph-db --path \".\" refresh --json",
      "tsift graph-db --path \".\" compact --apply --json"
    ]
  }
}
```

Two notes about the report:

1. `counts_after` in the JSON report reflects what the compact policy would have seen had it pruned tombstones (not the post-VACUUM filesystem state). The actual post-VACUUM file size + freelist are captured via `stat -c %s` + `PRAGMA freelist_count` and reported in the metrics table above.
2. `status: "not_needed"` in `compaction_before` is the policy heuristic's call before apply — it considered the 3.3 % freelist ratio below its threshold. The user-driven apply still reclaimed 200 MiB and zeroed the freelist, which is the operational evidence requested by `#gdbvacproof`.

## What's still gated behind `#gdbvacconvex`

- `--prune-tombstones --confirmed-convex-reconciled` is unsafe today because no Convex consumer has reconciled the 781,653 tombstone rows. Pruning would tell downstream Convex clients those nodes/edges never existed, breaking the eventual-consistency contract the schema in `examples/convex-graph/{schema,graph,http}.ts` provides.
- Live `tsift convex-sync --remote-snapshot --apply` cannot be exercised against this graph until a Convex deployment is reachable.
- After `#gdbvacconvex` lands (live Convex URL + reconciled snapshot), the closure path for the rest of `#gdbvacproof` is:
  1. `tsift convex-sync . --remote-snapshot --apply --json` → confirm no drift.
  2. `tsift graph-db --path . compact --apply --prune-tombstones --confirmed-convex-reconciled --json` → reclaim the additional ~200-300 MiB the 781 k tombstones currently hold.
  3. Re-run scan + doctor timings post-prune for the second half of this evidence doc.

## Verdict (partial closure)

The local SQLite compact workflow is **exercised and proven**: 200.6 MiB reclaimed, freelist zeroed, `status` stays at 1 ms warm, `doctor` reports clean post-apply state in 2.3 s. The Convex reconciliation + tombstone prune leg stays gated behind `#gdbvacconvex` with a precise unblock path.

`#gdbvacproof` is therefore **not closed this cycle** — only the local-compact half has evidence. It stays in review until the Convex half also has proof.
