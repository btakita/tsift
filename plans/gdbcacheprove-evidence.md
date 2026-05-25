# #gdbcacheprove evidence

Closing the four-part proof requested by review item `#gdbcacheprove`.

## 1. Post-fix real graph-db backend-eval --full-projection cold/cache pair on agent-loop

Covered by `#gdbfullreuse` (commit `tsift@23f0d52`). Three cold + three cache samples ran against the agent-loop superproject from `/home/brian/work/btakita/agent-loop`. Pair 1 hit cache cleanly with phase-split timing:

| Leg | SQLite total (µs) | `replace_projection_total` (µs) | `cache_load` (µs) | `source_graph_build` (µs) | `projection_rows` (µs) |
| --- | ---: | ---: | ---: | ---: | ---: |
| Cold | 17,908,886 | 10,763,684 | (write side ~26k) | 17,535,468 | ~2,800,000 |
| Hit  | 15,324,342 |  9,894,578 | 1,064,350 | 0 | 0 |

Raw JSON at `target/perf/al-{cold,cache}-{1,2,3}.json`. Pairs 2 and 3 reverted to full rebuild because sibling subagents edited tracked source files during the 25-minute sample window — that is environmental noise (real source touches do invalidate the cache, as designed), not a regression in the cache path.

## 2. `.agent-doc/` runtime markdown does not change the source watermark

The single chokepoint for source-scan exclusion is `traversal_relative_path_is_generated_artifact` at `src/main.rs:12868`. It excludes any path that:

- equals `.agent-doc`
- starts with `.agent-doc/`
- ends with `/.agent-doc`
- contains `/.agent-doc/`

All source-watermark consumers route through `traversal_path_is_generated_artifact` (the helper one frame up) which applies the same rule against both the project root and the source root relative path. New unit test `traversal_excludes_agent_doc_runtime_paths_from_source_watermark` (in `mod tests` of `src/main.rs`) regression-locks the rule across all the typical `.agent-doc/` runtime artifact paths (snapshots, baselines, archives, runtime JSONL, session docs at any depth) and asserts real source paths are NOT excluded.

Operational corroboration: the `#gdbfullreuse` cycle ran the agent-doc workflow itself between sample pairs (snapshot writes, baseline writes, commit boundary), and pair 1 still cache-hit. The cache miss on pairs 2 and 3 was traced to sibling agents editing `src/main.rs` and similar tracked files — not `.agent-doc/` activity.

## 3. Prune stale pre-fix `.tsift/backend-eval-cache` miss artifacts

Inspected both cache directories after the `#gdbfullreuse` cycle:

```
/home/brian/work/btakita/agent-loop/.tsift/backend-eval-cache/full_projection/
  f73c0dcd95c999d252c3a451c3b39016e7aa1168995638a01b0ce187d5ec29a2.json.gz  (90.6M, fresh)

/home/brian/work/btakita/agent-loop/src/tsift/.tsift/backend-eval-cache/full_projection/
  2c51b14f96614a859dd2d120f37d1fdbc82edea7f0be853ee68cb6f283181da0.json.gz  (2.3M, fresh)
```

Each cache directory holds exactly one entry, written by the most recent `#gdbfullreuse` sample run. No stale pre-fix miss artifacts remain — the LRU eviction baked into `pruned_kept_with_size` already collapsed prior artifacts. Nothing to prune; the cache surface is clean.

## 4. Refresh `fixtures/graph-db-performance-history.json` with a cache-hit sample

Covered by `#gdbfullreuse` (commit `tsift@23f0d52`). Nine workload-tagged entries appended: `workload="full-projection"`, `sample_index 1..3`, `backend="sqlite"`, `projection_mode="full"`, `cache_state in {cold,hit,miss}`, `scope in {tsift-submodule,agent-loop-superproject}`.

## Verdict

`#gdbcacheprove` closed. Real cold/cache evidence, watermark exclusion proof (code path + new unit test + operational corroboration), cache-artifact prune verified clean, fixture refreshed.
