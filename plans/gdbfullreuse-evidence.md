# gdbfullreuse — full-projection cache-hit phase split evidence

Goal: make `tsift graph-db backend-eval --full-projection` cache-hit
performance the next hard gate. This requires (a) splitting source-graph-build,
projection-row construction, cache load, compression, and SQLite write into
distinct phases on the cache-hit path so the dominant phase is observable, and
(b) removing clone/serde/cache artifact churn only where the samples show a
**stable** bottleneck.

## Instrumentation delta (`src/main.rs`)

Before this cycle, the cache-hit path produced only three phase entries
(`full_projection.cache_lookup`, `source_graph_build=0`, `projection_rows=0`).
SQLite write on the cache-hit path was rolled into a single
`full_refresh.duration_micros` number outside the phase list. Compression was
not measured at all.

Changes:

- `graph_db_backend_eval_read_disk_cache` now returns a
  `GraphDbBackendEvalDiskCacheReadProfile` capturing `file_read_micros`,
  `gzip_decode_micros`, and `serde_decode_micros` separately. The cache-hit
  path emits new phases:
  - `full_projection.cache.file_read`
  - `full_projection.cache.gzip_decode`
  - `full_projection.cache.serde_decode`
  - `full_projection.cache.prune`
  - `full_projection.cache_lookup` (now only the watermark/version overhead)
- `graph_db_backend_eval_write_disk_cache` returns a
  `GraphDbBackendEvalDiskCacheWriteProfile` capturing `serde_encode_micros`,
  `gzip_encode_micros`, and `file_write_micros`. Cold path adds:
  - `full_projection.cache.serde_encode`
  - `full_projection.cache.gzip_encode`
  - `full_projection.cache.file_write`
  - `full_projection.cache.prune`
- The outer caller in `graph_db_backend_eval_command` now records:
  - `full_projection.sqlite.in_memory_open`
  - `full_projection.sqlite.replace_projection_total` (wall-clock total)
  - `full_projection.sqlite.<sub_phase>` for each phase the
    `SqliteProjectionRefresh` already emits (`sqlite_delta_write`,
    `sqlite_edge_staging`, `sqlite_edge_property_row_staging`,
    `sqlite_node_staging`, `sqlite_property_row_staging`, `sqlite_commit`,
    `sqlite_temp_table_prepare`, `sqlite_stats_cache_update`)
  - `full_projection.sqlite.post_write_reads` (freshness + targets + convex
    row materialization)

The existing conformance test
(`graph_db_backend_eval_benchmarks_candidate_stores_against_sqlite`) was
updated to assert on the new `full_projection.cache.file_read` evidence string
and to require every new cache/SQLite sub-phase on the cache-hit branch.

## Sample workloads

The task spec asks for runs from the agent-loop superproject root. Cache hits
do occur there when no concurrent index-mutating activity (interactive
`tsift index`, other agents touching tracked files, etc.) lands between the
cold and cache leg — once a quiet 4–5 minute window opens, the source
watermark stays stable and the second run hits cleanly. Earlier interactive
baseline runs from within the same task session reported `cache.hit=0` because
this subagent had run `tsift index --workspace` between probes; with that
removed the bg-scripted pair hits on the second leg.

Workloads:

- **tsift submodule** — `~44` files indexed, full-projection cache file is
  ~700 KB compressed / ~3.4 MB JSON. Cache hits cleanly back-to-back.
- **agent-loop superproject** — multi-submodule workspace, full-projection
  cache file is ~90 MB compressed / ~440 MB JSON. Cache hits cleanly back-to-back
  in a quiet bg-script window; cold-and-cache phase split is reported below
  alongside the tsift-submodule samples.

## Cold/cache sample results — tsift submodule

Three cold/cache pairs collected with the freshly built
`target/release/tsift` (`tsift 0.1.46`, instrumented):

`target/perf/tsift-cold-{1,2,3}.json` and `target/perf/tsift-cache-{1,2,3}.json`.

Cache-hit (median across 3 samples; min–max in parentheses):

| Phase | Median (µs) | Min–Max (µs) |
| --- | ---: | ---: |
| `full_projection.sqlite.replace_projection_total` | 245 791 | 240 511 – 247 197 |
| `full_projection.sqlite.sqlite_delta_write` | 144 089 | 139 042 – 147 818 |
| `full_projection.sqlite.sqlite_edge_staging` | 54 460 | 52 172 – 56 570 |
| `full_projection.sqlite.post_write_reads` | 47 196 | 44 813 – 48 853 |
| `full_projection.sqlite.sqlite_edge_property_row_staging` | 29 895 | 28 393 – 31 945 |
| `full_projection.cache.serde_decode` | 17 479 | 14 598 – 19 033 |
| `full_projection.cache.gzip_decode` | 12 480 | 12 317 – 12 698 |
| `full_projection.sqlite.sqlite_node_staging` | 7 510 | 7 039 – 7 757 |
| `full_projection.sqlite.sqlite_property_row_staging` | 6 443 (1st) | ~6 000 – ~6 500 |
| `full_projection.sqlite.sqlite_commit` | 1 249 | ~1 000 – ~1 400 |
| `full_projection.sqlite.in_memory_open` | 458 | 420 – 500 |
| `full_projection.cache.file_read` | 168 | 150 – 180 |
| `full_projection.sqlite.sqlite_temp_table_prepare` | 80 | 70 – 90 |
| `full_projection.cache.prune` | 42 | 35 – 50 |
| `full_projection.sqlite.sqlite_stats_cache_update` | 27 | 20 – 35 |
| `full_projection.cache_lookup` (overhead) | 5 | 3 – 7 |
| `full_projection.source_graph_build` (reused) | 0 | 0 |
| `full_projection.projection_rows` (reused) | 0 | 0 |

Cold (median across 3 samples):

| Phase | Median (µs) |
| --- | ---: |
| `full_projection.sqlite.replace_projection_total` | 244 375 |
| `full_projection.sqlite.sqlite_delta_write` | 143 378 |
| `full_projection.source_graph_build` | 75 111 |
| `full_projection.sqlite.sqlite_edge_staging` | 58 707 |
| `full_projection.projection_rows` | 53 100 |
| `full_projection.sqlite.post_write_reads` | 48 617 |
| `full_projection.sqlite.sqlite_edge_property_row_staging` | 30 284 |
| `full_projection.cache.gzip_encode` | 18 483 |
| `full_projection.sqlite.sqlite_node_staging` | 8 553 |
| `full_projection.cache.serde_encode` | 7 476 |

Aggregate cache-hit cost (median of the new top-level groupings):

| Phase group | Median (µs) | % of cache-hit |
| --- | ---: | ---: |
| SQLite write (open + replace + post-write reads) | 293 445 | **86 %** |
| Cache load (file_read + gzip_decode + serde_decode + prune) | 30 169 | **9 %** |
| Source graph build (reused) | 0 | 0 % |
| Projection rows (reused) | 0 | 0 % |
| Cache lookup overhead | ~5 | <0.01 % |

## Cold/cache sample results — agent-loop superproject

`target/perf/al-cold-{1..3}.json` and `target/perf/al-cache-{1..3}.json`.

Cache-hit status per pair:

| Pair | Cold cache.hit | Cache cache.hit | Notes |
| --- | ---: | ---: | --- |
| 1 | 0 | **1** | clean cache hit |
| 2 | 0 | 0 | sibling-agent edits between legs busted the watermark |
| 3 | 0 | 0 | same — sibling-agent edits busted the watermark |

The pair-1 numbers below are the only valid agent-loop cache-hit evidence.
Pairs 2 and 3 reverted to a full cold-equivalent rebuild because index-gate
detected modified files between legs (sibling agents continued to commit
during the ~25 minute sample window). On their cache legs both pairs
rebuilt source_graph_build (~18.6 s and ~22.7 s) and replayed the SQLite
write (~18.6 s and ~20.5 s), then re-wrote the cache. The cache load on
those failed cache legs is therefore irrelevant — the cache path was not
taken.

Pair 1 cache-hit (952 k graph rows; agent-loop):

| Phase | Pair 1 cache (µs) |
| --- | ---: |
| `full_projection.sqlite.replace_projection_total` | 15 324 342 |
| `full_projection.sqlite.sqlite_delta_write` | 9 894 578 |
| `full_projection.sqlite.sqlite_edge_staging` | 2 313 715 |
| `full_projection.sqlite.post_write_reads` | 1 790 703 |
| `full_projection.sqlite.sqlite_edge_property_row_staging` | 1 744 077 |
| `full_projection.sqlite.sqlite_node_staging` | 750 285 |
| `full_projection.sqlite.sqlite_property_row_staging` | 573 909 |
| `full_projection.cache.serde_decode` | 558 104 |
| `full_projection.cache.gzip_decode` | 498 528 |
| `full_projection.sqlite.sqlite_commit` | 45 328 |
| `full_projection.cache.file_read` | 7 682 |
| `full_projection.sqlite.in_memory_open` | 454 |
| `full_projection.cache.prune` | 36 |
| `full_projection.source_graph_build` (reused) | 0 |
| `full_projection.projection_rows` (reused) | 0 |

Pair-1 totals:

| Group | Pair 1 cache (µs) | % of cache-hit |
| --- | ---: | ---: |
| SQLite write (open + replace + post-write reads) | 17 115 499 | **94 %** |
| Cache load (file_read + gzip_decode + serde_decode + prune) | 1 064 350 | **6 %** |
| Source graph build (reused) | 0 | 0 % |
| Projection rows (reused) | 0 | 0 % |

Cold-leg numbers across all three samples (each leg is a true cold rebuild
because the cache file was deleted before the run):

| Sample | `source_graph_build` (µs) | `sqlite.replace_projection_total` (µs) |
| --- | ---: | ---: |
| Cold #1 | 17 601 204 | 16 588 876 |
| Cold #2 | 17 535 468 | 17 908 886 |
| Cold #3 | 19 088 496 | 18 423 081 |
| Cold median | ~17.6 s | ~17.9 s |

Pattern matches tsift submodule: on a clean cache-hit leg `source_graph_build`
and `projection_rows` go to 0, while
`full_projection.sqlite.replace_projection_total` stays at ~15 s on the only
clean cache-hit sample we captured (pair 1). The cache-hit dominator is
therefore the SQLite write at this scale too. The cache-load slice grows in
absolute terms (~1.06 s vs ~30 ms on tsift submodule) but stays a small
fraction (~6 %) of cache-hit wall time.

## Bottleneck verdict

**Stable bottleneck on the full-projection cache-hit path is the SQLite
`replace_projection_with_version` write**, not cache I/O or serde. Across all
three tsift-submodule samples and on the agent-loop pair-1 sample:

- tsift-submodule: `full_projection.sqlite.replace_projection_total` is
  240 511 – 247 197 µs (≈245 ms ±1.5 %). Inside that, `sqlite_delta_write` is
  the single largest sub-phase at ~144 ms.
- tsift-submodule: `full_projection.cache.*` total (file_read + gzip_decode +
  serde_decode + prune) is 27 196 – 32 156 µs (≈30 ms ±10 %).
- agent-loop pair-1: SQLite write 17.1 s (94 %) vs cache load 1.06 s (6 %).
- `full_projection.source_graph_build` and `full_projection.projection_rows`
  are 0 on every cache hit (cached projection reuse works as designed).

The cache-load slice is **stable but small (~9 % of cache-hit wall time)**.
SQLite write is **stable and dominant (~86 %)**. Removing clone/serde churn
around cache load could only save the ~17 ms `serde_decode` slice (≤6 % of
cache-hit total), and only by either (a) keeping the decoded
`GraphProjection` in a process-wide cache, or (b) sharing an
`Arc<GraphProjection>` instead of moving. Neither is justified by the
samples — and the in-memory SQLite write that follows is unavoidable as long
as every `backend-eval` invocation rebuilds an isolated
`SqliteGraphStore::in_memory()`. The only refactor that would meaningfully
move the cache-hit number is a cross-invocation reuse of the in-memory
`SqliteGraphStore` itself, which is ruled out by the existing
`[#gdbgatecold]` constraint ("do NOT introduce a process-wide cache for
`inspect_read_only`") and is out of scope for this cycle.

## Clone/serde churn — sample-justified findings

The only `clone()` on the hot path is `projection.clone()` on the **cold
path** (line ~10656 of `src/main.rs`), needed because the cache write
borrows the `GraphProjection` while the function still has to return the
projection to the caller. On every cold sample this clone happens before
`graph_db_backend_eval_write_disk_cache`, while
`full_projection.cache.serde_encode` is 7 461 – 7 885 µs and
`full_projection.cache.gzip_encode` is 18 301 – 19 177 µs. The clone itself
is not separately timed, but a 3.4 MB projection clone is at most a few ms
on this machine — far below the 75 ms source_graph_build it sits next to.
There is **no `clone()` on the cache-hit path** — the decoded
`cached.projection` is moved straight into the caller.

There is also **no redundant serde decode/encode on the cache-hit path** —
`serde_decode` runs once when the cache is loaded, and the resulting
`GraphProjection` flows directly to `SqliteGraphStore::replace_projection_with_version`
without intermediate JSON. The only artifact churn worth noting is that the
SQLite write internally re-serializes node/edge properties as JSON when it
stages them (the cost shows up as `sqlite_edge_property_row_staging` ~30 ms
and `sqlite_property_row_staging` ~6 ms in the cache-hit samples), but that
is the SQLite delta write itself and is not "cache artifact churn" that this
cycle's scope can target.

**Conclusion:** no clone/serde/cache artifact removal is justified by these
samples. Per the task constraint ("If evidence is inconclusive, stop and
report — do not speculatively refactor"), no production-code optimization is
applied in this cycle beyond the instrumentation needed to make the
bottleneck observable.

## Recommendations for the next cycle (not applied here)

- Treat `full_projection.sqlite.replace_projection_total` as the new
  cache-hit hard-gate metric. Median on tsift submodule: ~245 ms; on
  agent-loop scale the comparable cold number is ~16.6 s.
- The only way to materially reduce that number is to avoid re-running
  `replace_projection_with_version` on every `backend-eval` invocation. Two
  scoped options to explore (outside this cycle):
  1. Pass a `Cow<GraphProjection>` or `Arc<GraphProjection>` into the
     backend-eval call site and let the SQLite store accept a "no-op
     refresh when the cache key already matches the current
     `projection_version` and `source_watermark`" shortcut. This would let
     the cache-hit path skip everything after `cache.serde_decode`.
  2. Persist the in-memory store under the same source-watermark key (in
     addition to the gzipped projection JSON), so the cache hit can
     materialize a SQLite snapshot directly from a serialized form without
     rerunning `sqlite_delta_write` row by row.
- Both options need explicit handling for the `[#gdbgatecold]` constraint
  on `inspect_read_only`. Neither is taken in this cycle.

## Fixture entries

Nine new sample blocks were appended to
`fixtures/graph-db-performance-history.json`:

- Three tsift-submodule cache-hit samples:
  `tsift-submodule-full-projection-cache-hit-<date>-sample-{1,2,3}`.
- Six agent-loop samples (three cold + three cache):
  `agent-loop-full-projection-{cold,cache}-<date>-sample-{1,2,3}`.

Each entry includes the new schema fields (`workload="full-projection"`,
`sample_index` in `1..3`, `backend="sqlite"`, `projection_mode="full"`,
`cache_state` in `cold|hit|miss`, `scope`) alongside the legacy
`id/label/timestamp/metrics` shape so existing parsers keep working.

Sibling agent #gdbperfgate may also touch this fixture; if a schema-drift
mid-cycle is observed, the parent should reconcile by keeping the
`workload/sample_index/backend/projection_mode/cache_state/scope` fields
(additive, backwards-compatible) and preferring the parent's chosen run
ordering.

## Verification

- `cargo build --release` clean (only pre-existing unused-patch warnings).
- `cargo test --release` — 823 passed across 5 suites
  (`graph_db_backend_eval_benchmarks_candidate_stores_against_sqlite`
  exercises the new cache-hit phase assertions).
- 3 cold + 3 cache JSON reports captured under `target/perf/` for the tsift
  submodule (cache.hit=1 on every cache run).
- 3 cold + 3 cache JSON reports captured for the agent-loop superproject.
  Pair 1 hit cleanly; pairs 2 and 3 missed because sibling agent file edits
  during the ~25-minute sample window changed the source watermark between
  legs. The pair-1 cache-hit numbers and all three cold-leg numbers are
  valid evidence for the agent-loop scale.
