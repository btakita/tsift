# #gfullhot evidence

Backend-eval full-projection hot-path proof for `#gfullhot`.

## Change

The full-projection backend-eval cache key now uses a stable input watermark over:

- indexed symbol rows
- indexed call-edge rows
- indexed route rows
- semantic summary rows

It intentionally leaves path-only index churn and agent-doc session markdown churn to the bounded real dataset. Full-projection is the large code/summary topology guard, so cache reuse is tied to symbol, call-edge, route, and summary inputs rather than every file-state or task-document edit in the workspace.

The cache lookup phase now prints the component hashes in its detail field, so misses can identify whether symbols, call edges, routes, summaries, or the final watermark changed.

## Tests

Focused regression tests:

```text
cargo test full_projection_
```

Covered cases:

- mtime-only source/index churn does not change the full-projection source watermark.
- unrelated agent-doc session markdown churn does not change the full-projection source watermark.
- a full-projection cache hit reports `full_projection.source_graph_build=0` and `full_projection.projection_rows=0`.

## Backend-Eval Samples

Command:

```text
cargo run -- graph-db --path /home/brian/work/btakita/agent-loop/tasks/software/tsift.md --json backend-eval --full-projection --candidate kuzu --target gfullhot
```

Raw local artifacts:

- `target/perf/gfullhot-full-projection-3.json`
- `target/perf/gfullhot-full-projection-4.json`
- `target/perf/gfullhot-full-projection-5.json`

| Sample | Cache hit | `source_graph_build` us | `projection_rows` us | `cache_lookup` us | `cache.file_read` us | `cache.serde_decode` us | Graph rows |
| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| 3 | 0 | 81,160,531 | 20,690,098 | 23 | 0 | 0 | 995,002 |
| 4 | 1 | 0 | 0 | 1,473 | 7,704 | 5,413,348 | 995,002 |
| 5 | 1 | 0 | 0 | 2,578 | 7,146 | 5,691,639 | 995,002 |

Samples 4 and 5 used the same stable component hashes:

```text
symbols=3b8e7dcaa821f03766d7f3f08631fc20690c5496bbe2b03f236a776ba016b6e8
call_edges=74cc61871740b2a3a7a4c7abd70abcd54a97b7f6a811554a42c539e2d17f7303
routes=d3c3f7feea71f8b4d20bb11408640f8426fae0fb05375634ff9853c57e9522ef
summaries=5c6c37890eeab8dd5361dc9dc4ad10bf081b48e5aed19dab37571e94b36ca407
watermark=dd83f3370fa1e9a5da31de4e5853f0bcf08e84461e7ba5fba321c8d2acad1688
```

## Verdict

`#gfullhot` is code-complete: repeated full-projection backend-eval runs now skip the source graph and provider-neutral projection-row rebuild when the symbol/call-edge/route/summary inputs are unchanged, while still reporting cache load costs separately.
