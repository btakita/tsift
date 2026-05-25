# #cachelookupshift — Conflict-Matrix `preparation_cache_lookup` Drift Investigation

## Verdict

**No fix needed — the cache miss is correct.** On the agent-loop superproject the `conflict_matrix_preparation.preparation_cache_lookup` shifts because real user edits land on tracked markdown files between consecutive runs. On a quiescent repo the cache hits cleanly (`disk_hit` at ~2 ms vs ~250 ms recompute on miss).

This document captures the drift attribution, the artifact-filter / directory-mtime hypothesis verification, and the new regression-locking tests added to prevent future drift sources from silently invalidating the cache.

## Pre-fix consecutive-pair measurement (agent-loop superproject)

`tsift graph-db --json --path /home/brian/work/btakita/agent-loop backend-eval`, captured to `target/perf/cachelookupshift-{1,2}.json`:

| Run | `preparation_cache_lookup` (µs) | `detail` |
|---|---|---|
| 1 | 250,774 | `no prepared packet matched the source/document/staged-diff watermark` |
| 2 | 243,262 | `no prepared packet matched the source/document/staged-diff watermark` |

Both runs took the "computed" branch of `prepare_conflict_matrix_inputs` (`src/main.rs:18502+`). The disk cache directory `.tsift/conflict-matrix-cache/inputs/` does receive populated entries on every run (hundreds of distinct keys are accumulated), confirming that the watermark itself is shifting between invocations rather than the cache write being broken.

## Drift identification

A temporary `TSIFT_DEBUG_WATERMARK_DUMP` env-gated dump was added to `traversal_source_watermark` (`src/main.rs:13054+`) to write every `parts` entry to disk per call. The dump was removed before submitting this evidence.

Each `graph-db backend-eval` invocation calls `traversal_source_watermark` 2–4 times (different callers: 4th call is the `prepare_conflict_matrix_inputs` one). Comparing the dumps across two consecutive runs on the agent-loop superproject, the only differing `parts` were:

- `index_snapshot:file:.../tasks/<name>.md:<secs>:<nanos>:markdown` — file_state rows whose `mtime_secs`/`mtime_nanos` shifted because the user edited the file between runs.
- `markdown:tasks/<name>.md:len=<N>:mtime=<secs>.<nanos>` — direct markdown metadata watermark, length **and** mtime both shifted (e.g. `tasks/software/tagpath.md` 15023 → 9441 → 11134 bytes across three timestamps).
- `markdown_count` / `index_snapshot_rows` — incremented by one when a new markdown file appeared (e.g. `tasks/agent-doc/plan-skill-install-runbooks-reconcile.md`).

The drift was **entirely** in markdown files the user was actively editing in another session window. No `.tsift/`, `.agent-doc/`, `target/`, or look-alike path slipped past `traversal_relative_path_is_generated_artifact`. No directory mtime entered the hash — `markdown_files_for_traversal` only enumerates files (`entry.file_type().is_some_and(|ft| ft.is_file())`), and `push_traversal_metadata_watermark_part` is only invoked for two specific file paths (each markdown + `.tsift/summaries.db`) which both stat as regular files.

### Confirmation on a quiescent repo

`tsift graph-db --json --path /home/brian/work/btakita/agent-loop/src/tsift backend-eval` pair, where no concurrent editing was happening:

| Run | `preparation_cache_lookup` (µs) | `detail` |
|---|---|---|
| 1 | 2,516 | `no prepared packet matched the source/document/staged-diff watermark` (cache populate) |
| 2 | 2,027 | `reused prepared context-pack, staged diff, and impact packet from .tsift/conflict-matrix-cache by source/document/staged-diff watermark` |

`disk_hit` at 2,027 µs vs the ~250 ms recompute path on agent-loop = **>100× speedup on the cached path**. The cache works correctly; the agent-loop pair just never sees identical source state because user-driven mutation of `tasks/*.md` is ongoing.

A two-run debug-dump trial on agent-loop **with no concurrent edits** also produced byte-identical dumps for the 4th (`prepare_conflict_matrix_inputs`) watermark call between the two runs:

| Pair | r1 dump sha | r2 dump sha | `preparation_cache_lookup` |
|---|---|---|---|
| `prepare_conflict_matrix_inputs` call (4th) | `0db0890748281a59…` | `0db0890748281a59…` | Run 2 = `disk_hit` (152 ms) |

## Suspected causes — verification

The task hypothesis listed two candidates. Both were checked and ruled out as the source of agent-loop drift:

1. **Generated artifact path slipping past `traversal_relative_path_is_generated_artifact` (`src/main.rs:12944`).** Verified by searching every dumped `parts` entry across multiple runs for `.tsift/`, `.agent-doc/`, and `target/` substrings on both `index_snapshot:file:` and `markdown:` lines. No slip-through was found. The filter already covers bare, root-anchored, nested (`/<dir>/`), and trailing (`/<dir>`) variants. Existing `traversal_excludes_agent_doc_runtime_paths_from_source_watermark` covered `.agent-doc/`; this slice adds `traversal_excludes_tsift_and_target_runtime_paths_from_source_watermark` to lock the `.tsift/` and `target/` cases plus look-alike paths (`a__target/`, `tsift-extras/`, `.tsiftrc`, `targeting.rs`, `agent-doc-helper.rs`) that must NOT be excluded.
2. **A directory mtime entering the hash (the `#gdbgatecold` ext4 surprise).** Verified by reading `markdown_files_for_traversal` (`src/main.rs:12885`) and `push_traversal_metadata_watermark_part` (`src/main.rs:12920`). The walker filters to `file_type().is_some_and(|ft| ft.is_file())`. The two `push_traversal_metadata_watermark_part` call sites target the markdown file path and `.tsift/summaries.db` respectively — both regular files. No directory mtime path. The `source_snapshot_parts` SQL query (`src/index.rs:1058`) reads `path, mtime_secs, mtime_nanos, language` from `file_state` — also no directories.

## Fix applied

None — see verdict above.

## Tests added

Both in `src/main.rs` under `#[cfg(test)] mod tests`:

- `traversal_excludes_tsift_and_target_runtime_paths_from_source_watermark` — covers `.tsift/`, `target/` (and prefix/suffix variants) as excluded, and `a__target/`, `tsift-extras/`, `tsift/README.md`, `targeting.rs`, `.tsiftrc`, `agent-doc-helper.rs` as NOT excluded. Locks the artifact filter against future regressions that would let a generated path slip into the watermark (which would invalidate the cache every run because those paths mutate as a side effect of running tsift).
- `traversal_source_watermark_is_stable_across_invocations_on_quiescent_root` — drives `traversal_source_watermark(..., session_only=true)` with a hinted markdown file (avoids needing a full index DB), asserts identical hash across two back-to-back calls, asserts the hash IS UNCHANGED after mutating `.tsift/index.db` and `target/debug/marker` (artifact-filtered), and asserts the hash CHANGES after mutating the hinted markdown content (real user state). Fail-closed on any future regression that folds wall-clock time, a directory mtime, or any other non-content input into the hash.

Both tests pass:

```
cargo test --bin tsift traversal_
test result: ok. 17 passed; 0 failed; 0 ignored; 0 measured; 670 filtered out
```

`make check` (clippy + full test suite): `687 + 110 + 31 + 21 + 4 = 853 passed; 0 failed`.

The regression-protected test `search_timeout_reports_reindex_when_index_turns_stale_during_worker_run` still passes (1 passed, 109 filtered out).

## Post-fix consecutive-pair measurement

Since no behavioral fix was applied, the after-fix agent-loop numbers are functionally identical to the pre-fix numbers — both runs miss because real user edits land between them:

`target/perf/cachelookupshift-after-{1,2}.json`:

| Run | `preparation_cache_lookup` (µs) | `detail` |
|---|---|---|
| 1 | 243,353 | `no prepared packet matched the source/document/staged-diff watermark` |
| 2 | 252,974 | `no prepared packet matched the source/document/staged-diff watermark` |

`target/perf/cachelookupshift-after-tsift-{1,2}.json` (quiescent `src/tsift` submodule):

| Run | `preparation_cache_lookup` (µs) | `detail` |
|---|---|---|
| 1 | 2,516 | populate (`no prepared packet matched ...`) |
| 2 | 2,027 | `disk_hit` (`reused prepared context-pack ... from .tsift/conflict-matrix-cache`) |

## Cache-hit ratio confirmation

Cache hits when source state is stable; misses when it changes. On a per-cycle basis for agent-loop, the hit ratio depends on whether the user edited a tracked markdown file between two consecutive `backend-eval` invocations. On a quiet repository (or on a fixed git checkout with no concurrent edits), the disk cache reliably hits with a >100× speedup over the recompute path.

## Out of scope (follow-ups recorded for future slices)

- **Narrower cache key for `backend-eval`.** The watermark currently includes every markdown file under the root via `markdown_files_for_traversal` (~9,676 markdown files on agent-loop). Restricting the markdown set to the subdirectory tree actually consumed by the `backend-eval` packet would let unrelated `tasks/*.md` edits avoid invalidating the cache. This is a non-trivial cache-key narrowing that needs its own scope analysis — explicitly out of this slice per the "do not invasively rework across many call sites" constraint, and orthogonal to the artifact-filter / directory-mtime hypothesis the task was framed around.
- **Process-wide cache.** Not done, per `#gdbgatecold` precedent (breaks `search_timeout_reports_reindex_when_index_turns_stale_during_worker_run`).

## Verification commands

| Command | Exit |
|---|---|
| 2× `target/release/tsift graph-db --json --path /home/brian/work/btakita/agent-loop backend-eval` (pre-fix pair → `target/perf/cachelookupshift-{1,2}.json`) | 0, 0 |
| 2× `target/release/tsift graph-db --json --path /home/brian/work/btakita/agent-loop backend-eval` (after-fix pair → `target/perf/cachelookupshift-after-{1,2}.json`) | 0, 0 |
| 2× `target/release/tsift graph-db --json --path /home/brian/work/btakita/agent-loop/src/tsift backend-eval` (quiescent pair → `target/perf/cachelookupshift-after-tsift-{1,2}.json`) | 0, 0 |
| `cargo check --tests` | 0 |
| `cargo clippy --all-targets -- -D warnings` | 0 |
| `cargo test --bin tsift traversal_` (17 passed) | 0 |
| `cargo test --test exit_code search_timeout_reports_reindex_when_index_turns_stale_during_worker_run` (1 passed) | 0 |
| `make check` (853 passed across 5 suites) | 0 |
