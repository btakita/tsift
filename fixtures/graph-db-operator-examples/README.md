# Graph DB Operator Examples

These examples document the SQLite and Convex graph DB paths with reusable commands and small fixtures.

## Graph Orchestration Contracts

`graph-orchestration-contracts.json` is a compact fixture for the versioned JSON
fields that agent-doc should consume:

- `graph-db-evidence-v1` for `graph-db evidence` packets with `packet_id`,
  `projection_hash`, `worker_results`, `replay_commands`, and `repair_commands`
- `worker-prompt-packet-v1` for conflict-matrix `worker_prompt_packets`
- `conflict-matrix-v1` for planner decisions keyed by evidence packet ids and
  worker-feedback closure ranking controls
- `context-pack-graph-orchestration-v1` for context-pack freshness/evidence
  observability
- `session-review-follow-up-v1` for session-review next-context commands
- `dispatch-trace-v1` for JSON/HTML replay views over backlog, job_packet,
  worker_result, worker feedback, source_handle, and worker_prompt_packet rows
- `dependency-dag-v1` for topological batches, cycle diagnostics, and replayable
  backlog dependency edges from explicit text, overlaps, semantics, and
  worker-result follow-up ids

```bash
tsift graph-db --path . schema --json
cat fixtures/graph-db-operator-examples/graph-orchestration-contracts.json
cat fixtures/graph-db-operator-examples/agent-orchestration-acceptance-pack.json
```

`agent-orchestration-acceptance-pack.json` is the golden agent-doc queue fixture:
it contains sample queue lines, job_packet rows, worker_result rows,
conflict-matrix/dispatch-trace replay commands, stale Convex repair commands,
and the context-pack / session-review follow-up commands that agent-doc should
consume. Its `regenerated_samples` block is produced by the graph conformance
test from the same queue fixture, covering `graph-db refresh`, `status`,
`doctor`, `evidence`, stale Convex `drift`, stale `convex-sync`,
`conflict-matrix`, `dispatch-trace` JSON/HTML, `context-pack`, and
`session-review`; CI fails when those normalized live outputs drift from the
checked-in pack.

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

## Graph-Backed Dispatch Path

Run the operator path in order when preparing parallel worker dispatch:

```bash
tsift graph-db --path . refresh --json
tsift graph-db --path . evidence gsch --depth 3 --limit 8 --json
tsift conflict-matrix --path tasks/software/tsift.md gsch grev wres --json
tsift dispatch-trace --path tasks/software/tsift.md gsch grev wres --json
tsift dispatch-trace --path tasks/software/tsift.md gsch --format html
tsift dependency-dag --path tasks/software/tsift.md gsch grev wres --json
tsift --envelope context-pack tasks/software/tsift.md --budget normal --json
tsift session-review tasks/software/tsift.md --next-context --json
```

Use evidence `packet_id` plus `projection_hash` as replay proof. If a packet or
conflict-matrix input reports stale/missing freshness, stop dispatch and run the
emitted `repair_commands` before trusting the worker ownership blocks.
Worker-feedback closure warnings for repeated blockage, stale expected tests,
or follow-up debt are soft ranking signals; they can reorder equal-risk safe
candidates, but shared file/symbol/test/config gates remain authoritative.

The acceptance pack records the full operator replay spine. `graph-db refresh`
is the only mutating local graph step; `graph-db status` must replay its counts
without refreshing, `graph-db doctor` must stay read-only, and stale Convex
snapshot reads must go through `drift` / `doctor` diagnostics before any
`graph-db --backend convex-snapshot ...` read is trusted.

## Convex Sync Dry Run

```bash
tsift graph-db --path . \
  --backend convex-snapshot \
  --convex-snapshot fixtures/graph-db-operator-examples/stale-convex-snapshot.json \
  drift \
  --json

tsift graph-db --path . \
  --backend convex-snapshot \
  --convex-snapshot fixtures/graph-db-operator-examples/stale-convex-snapshot.json \
  doctor \
  --json

tsift convex-sync . --snapshot fixtures/graph-db-operator-examples/stale-convex-snapshot.json --chunk-size 25 --json
```

The stale fixture intentionally omits the local projection metadata and Convex
index metadata while carrying one stale remote node and edge. `drift` and
`doctor` should fail closed, emit repair commands that point to
`convex-sync --snapshot ...` and `convex-sync --remote-snapshot --apply`, and
block Convex-backed graph reads until the plan is applied. `convex-sync` should
plan edge tombstones before node tombstones, then node upserts before edge
upserts.

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
Completed or blocked worker responses become `worker_result` nodes with status,
touched files, expected tests, and follow-up ids, then link back to backlog/job
handles and source windows for the next dispatch cycle.
