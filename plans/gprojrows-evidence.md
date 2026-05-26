# gprojrows — full-projection source/projection row reuse evidence

Goal: close `#gprojrows` by proving unchanged symbol and summary inputs reuse
the full-projection source graph and provider-neutral projection rows before
backend or hop-cap changes.

## Implementation status

The production optimization already exists in
`graph_db_backend_eval_full_projection_with_profile`: the full-project
projection is keyed by the generated-artifact-free source watermark and stored
under `.tsift/backend-eval-cache/full_projection` as compressed JSON. On a
matching cache key, backend-eval moves the cached `GraphProjection` into the
caller and emits:

- `full_projection.source_graph_build = 0`
- `full_projection.projection_rows = 0`

This skips code-index loading, session markdown scanning, source-handle
construction, semantic summary reads, and provider-neutral row construction on
cache hits.

This cycle tightened the contract rather than adding a speculative refactor:

- `SPEC.md` now states that full-projection cache hits must report both phases
  as `0us`.
- `tests/graph_db_conformance.rs` now asserts that a second
  `backend-eval --full-projection` run skips both phases.

## Fresh local samples

Commands ran from `/home/brian/work/btakita/agent-loop/src/tsift` after
`cargo install --path .`:

```bash
tsift graph-db --json backend-eval --full-projection > target/perf/gprojrows-warm.json
for i in 1 2 3; do
  tsift graph-db --json backend-eval --full-projection > target/perf/gprojrows-cache-$i.json
done
```

| Sample | `full_projection.cache.hit` | `source_graph_build` (us) | `projection_rows` (us) | `cache_lookup` metric (us) | SQLite total (us) |
| --- | ---: | ---: | ---: | ---: | ---: |
| `gprojrows-cache-1` | 1 | 0 | 0 | 3 | 302,627 |
| `gprojrows-cache-2` | 1 | 0 | 0 | 4 | 310,104 |
| `gprojrows-cache-3` | 1 | 0 | 0 | 4 | 304,030 |

Cache metadata was stable across the three measured hits:

- compressed cache bytes: `2,592,752`
- compression ratio: `0.2193532208921398`

## Verdict

`#gprojrows` is closed. The full-projection source graph and provider-neutral
projection rows are reused on unchanged source/summary inputs, the skip is
locked by conformance coverage, and three fresh `backend-eval
--full-projection` cache-hit samples prove both target phases remain at `0us`.
