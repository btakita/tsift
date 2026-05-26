# gcachemiss - full-projection cache reuse evidence

## Verdict

Full-projection cache reuse was unstable because the traversal source watermark
treated `.tsift/summaries.db` file metadata as source input. SQLite header,
checkpoint, or metadata churn can change that file's mtime without changing the
semantic summary rows that feed the traversal graph projection.

## Root cause

`traversal_source_watermark` already filters generated paths such as `.tsift/`,
`.agent-doc/`, and `target/` from index snapshot rows and markdown discovery, but
it appended a dedicated `summaries_db:<path>:len=<N>:mtime=<secs>.<nanos>` part
for `.tsift/summaries.db`. That made the full-projection cache key sensitive to
runtime SQLite metadata rather than semantic projection content.

## Fix

The watermark now opens `.tsift/summaries.db` read-only and hashes the stable
semantic fields used by `append_summary_semantic_projection_rows`:

- `symbol_name`
- `file_path`
- `entities`
- `relationships`
- `concept_labels`

If the summary database is absent, the watermark records `summaries_db:absent`.
If it cannot be read, the code falls back to metadata under an explicit
`summaries_db_unreadable` label so unreadable summary state still invalidates
conservatively.

## Gate update

The Graph DB performance release gate now treats full-projection cache misses as
diagnostic samples only. Hop-cap or backend promotion evidence must include a
cold populate leg followed by cache-leg samples with
`full_projection.cache.hit=1`.

## Regression coverage

- `traversal_source_watermark_uses_summary_rows_not_summaries_db_metadata`
  proves metadata-only SQLite summary-cache churn does not shift the source
  watermark, while semantic summary row changes still invalidate it.
- `graph_db_backend_eval_benchmarks_candidate_stores_against_sqlite` now asserts
  the serialized performance gate includes the full-projection cache-hit gate and
  backend adapter checks require cache-hit evidence before backend or hop-cap
  changes.

## Verification

Targeted regression tests:

| Command | Result |
| --- | --- |
| `cargo test --bin tsift traversal_source_watermark_uses_summary_rows_not_summaries_db_metadata` | pass |
| `cargo test --test graph_db_conformance graph_db_backend_eval_benchmarks_candidate_stores_against_sqlite` | pass |

Full local verification:

| Command | Result |
| --- | --- |
| `make check` | pass |
| `cargo build` | pass |
| `cargo install --path .` | pass |

CI review:

| Check | Result |
| --- | --- |
| `gh run list --workflow CI --limit 1` from `src/tsift` | green, run `26432096113` |
| root `Agent Doc` workflow `26432618478` | external CI-start blocker: `tmux-ci` and `check` jobs failed with empty step lists after runner startup; no logs indicate a code or tmux regression |

Operational full-projection cache pair from the agent-loop superproject:

```bash
tsift graph-db --path /home/brian/work/btakita/agent-loop backend-eval --full-projection
tsift graph-db --path /home/brian/work/btakita/agent-loop backend-eval --full-projection
```

Cold leg:

- `full_projection.cache_lookup 18us no full-project projection cache entry matched the source watermark`
- `full_projection.source_graph_build 25442659us`
- `full_projection.projection_rows 3115258us`

Cache leg:

- `source_graph_build 235us reused current graph.db projection because the source watermark matched`
- `conflict_matrix_preparation.preparation_cache_lookup 138832us reused prepared context-pack, staged diff, and impact packet from .tsift/conflict-matrix-cache`
- `full_projection.cache.file_read 9613us read compressed cache bytes from .tsift/backend-eval-cache`
- `full_projection.cache.gzip_decode 514623us`
- `full_projection.cache.serde_decode 568918us`
- `full_projection.source_graph_build 0us reused cached full-project source graph`
- `full_projection.projection_rows 0us reused cached provider-neutral full-project projection rows`
