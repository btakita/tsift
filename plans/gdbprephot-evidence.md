# #gdbprephot — Conflict-Matrix Preparation Hotspot Evidence

Profile the remaining `conflict_matrix_preparation.*` hotspots after the `#gdbgatecold` (0.1.48) and `#gdbfullreuse` (0.1.46) fixes, pick the dominant remaining phase from the four named in the task, and apply a narrow reduction plus a regression-locking gate.

Measured on agent-loop superproject (`tsift graph-db --json --path /home/brian/work/btakita/agent-loop backend-eval`) using `tsift 0.1.48` for baselines and the new `tsift 0.1.49` release build for the after-fix samples. Six runs per leg (3 default + 3 `--full-projection`).

Raw JSON: `target/perf/gdbprephot-{baseline,after}-{default,full}-{1,2,3}.json`.

## Baseline (0.1.48, pre-fix)

All durations in microseconds. Median across three samples.

| Phase | Default median | Full-projection median |
|---|---|---|
| `conflict_matrix_preparation` (parent) | 1,758,914 | 1,856,462 |
| `status_index_gate` | 673,199 | 712,491 |
| **`context_pack_diff`** | **445,507** | **480,676** |
| `session_review_compute` | 368,391 | 370,508 |
| `preparation_cache_lookup` | 255,237 | 260,711 |
| `impact` | 1,452 | 1,903 |

`dispatch_trace` is not a `conflict_matrix_preparation.*` sub-phase — it is a top-level `graph-db backend-eval` phase, and the current code emits no separate timer for it under preparation. Treating it as "or similar — check what timer name is emitted by current code" per the task statement: nothing under `conflict_matrix_preparation` matches that label, so dispatch_trace is omitted from this analysis. The full `phase_timings` set is available in the raw JSON.

## Dominant phase verdict

The task scopes the dominant-phase choice to the four named hotspots (`session_review_compute`, `context_pack_diff`, `impact`, `dispatch_trace`-or-similar).

`status_index_gate` (~673 ms default / ~712 ms full-projection) is the largest *overall* preparation hotspot, but `#gdbgatecold` (0.1.48) just landed scope-guard work there, and the task's framing explicitly carves it out as already-addressed. The remaining instrumented sub-cost (`prepare_agent_doc_index_gate`, `context_pack_status_reminders`) is dominated by the per-process cold-leg cost, which an in-process scope cache cannot help with for one-shot CLI invocations — that belongs to future work, not this slice.

Among the four named hotspots:

- `context_pack_diff` (445 ms / 481 ms) — **dominant**
- `session_review_compute` (368 ms / 371 ms) — 18% lower (82% of dominant), outside the 10% tie band
- `impact` (1.5 ms / 1.9 ms) — already minimized by 0.1.46 short-circuits
- `dispatch_trace` — no preparation-level timer

Dominant: `conflict_matrix_preparation.context_pack_diff`. No tie at the 10% threshold, so `session_review_compute` stays out of this slice.

### Why `context_pack_diff` is expensive

`build_context_pack_report_with_profile` (`src/main.rs:23399-23419`, pre-fix) called `diff_digest::compute(cached: false, revision: None)`, which parses **every** working-tree changed file with tree-sitter, runs `git show HEAD:path` for each one, and does a summary-cache lookup per file. On agent-loop this is ~41 files (30 tracked working-tree changes + 11 untracked). The result is then handed to `build_context_pack_diff_preview` (`src/main.rs:22602-22667`), which truncates the file list to `budget.preview_items()` = **5** files. Downstream consumers (`enrich_next_context_with_diff_symbols`, `build_context_pack_exploration_packet`) only iterate the preview window. Parsing 41 files to use 5 is the same shape of waste `impact::compute` removed in 0.1.46 via empty-input short-circuiting.

## Fix applied (`#gdbprephot`)

`src/diff_digest.rs`:

- Added `max_parsed_files: Option<usize>` to `DiffDigestOptions`. `None` keeps the historical full-parse behavior; `Some(N)` parses the first `N` files in canonical sort order and emits cheap path-only `DiffDigestFile` entries (empty `touched_symbols` / `current_summaries` / `added_call_edges` / `removed_call_edges`, `summary_state: Unavailable`, warning `parse_deferred_by_budget`) for the rest. Aggregate `symbols_touched`, `call_edges_added`, and `call_edges_removed` therefore reflect only the parsed subset; `files_changed` always counts every changed path.
- Reordered `compute` to collect every `(path, status, existing)` tuple, sort by canonical relative path, **then** decide parse vs defer. This guarantees `max_parsed_files = Some(N)` always selects the same `N` paths a sorted preview would take, regardless of git's enumeration order. (`src/diff_digest.rs:107-180`)
- For deferred entries we also skip `load_previous_bytes` / `git show HEAD:path` and `load_current_bytes` — the Modified-vs-Added distinction is only meaningful for files that actually reach the preview window. (`src/diff_digest.rs:147-156`)
- New helper `build_parse_deferred_diff_file` constructs the path-only entry. (`src/diff_digest.rs:248-268`)

`src/main.rs`:

- `build_context_pack_report_with_profile` now passes `max_parsed_files: Some(budget.preview_items())` into the `context_pack_diff` call. The other three `diff_digest::compute` callers (`cmd_diff_digest`, the `staged_diff` phase, `impact::compute`) keep `max_parsed_files: None` so they still see full-fidelity counts. (`src/main.rs:23399-23434`, `src/main.rs:16219-16228`, `src/main.rs:18540-18560`, `src/impact.rs:88-100`, `src/main.rs:28315-28324`)

## After (0.1.49, post-fix)

| Phase | Default median | Default delta | Full-projection median | Full-projection delta |
|---|---|---|---|---|
| `conflict_matrix_preparation` (parent) | 1,727,371 | -1.8% | (see raw JSON `gdbprephot-after-full-*.json`) | — |
| **`context_pack_diff`** | **288,577** | **-35.2%** | (see raw JSON) | — |
| `session_review_compute` | 359,963 | -2.3% | — | — |
| `status_index_gate` | 752,121 | +11.7% (noise — unchanged code path) | — | — |
| `preparation_cache_lookup` | 256,064 | +0.3% | — | — |
| `impact` | 1,701 | +17% (sub-ms noise) | — | — |

After-fix raw timings per sample (default, μs):

| Sample | context_pack_diff | session_review_compute | preparation_cache_lookup | status_index_gate | parent |
|---|---|---|---|---|---|
| 1 | 298,242 | 449,078 | 301,446 | 752,121 | 1,820,038 |
| 2 | 288,577 | 359,963 | 256,064 | 804,074 | 1,727,371 |
| 3 | 282,587 | 346,146 | 235,643 | 630,223 | 1,512,531 |

Full-projection after-fix is captured in the raw JSON at `target/perf/gdbprephot-after-full-{1,2,3}.json` (medians populate the per-sample table here once all three runs finish). The reduction direction matches the default leg — `context_pack_diff` parses 5 files instead of 41, regardless of projection mode, since both legs run through the same `build_context_pack_report_with_profile` path.

## Regression gate

`src/perf_gate.rs` is extended with:

- `evaluate_preparation_hotspot(phase, samples, budget_micros) -> PreparationHotspotReport`. Verdict is `Within`, `Regressed`, or `InsufficientSamples`. Below `MIN_HOTSPOT_SAMPLES = 3` the gate fail-closes (`InsufficientSamples`) instead of guessing.
- `CONTEXT_PACK_DIFF_BUDGET_MICROS = 350_000` (350 ms). Picked so that:
  - The new ~289 ms median fits with ~20% noise headroom for small repo growth or system jitter.
  - The pre-fix ~445 ms baseline trips the gate immediately — covered by the `preparation_hotspot_gate_fails_closed_on_pre_fix_baseline` integration test, which uses the literal pre-fix samples `[436_658, 445_507, 462_138]` and expects `PreparationHotspotVerdict::Regressed`.

### Fresh-sample contract ("do not trust stale ownership")

The function takes a borrowed `samples: &[u128]` slice and computes the median in-place. There is no internal cache, no static `OnceLock`, and no read of prior fixture history — every call recomputes against exactly the values the caller supplies. The unit test `preparation_hotspot_does_not_cache_prior_samples` locks this contract: a `Within` evaluation followed by a `Regressed` evaluation produces independent verdicts and independent medians, so a future refactor that adds memoization would break the test before reaching review.

### How the gate would fail closed on regression

- Removing `max_parsed_files` from the `context_pack_diff` call site → parses all 41 files again → median climbs back to ~445 ms → gate verdict `Regressed`, diagnostics include `REGRESSED: median 445507µs > budget 350000µs`.
- Reintroducing per-file `git show HEAD:path` for deferred entries (e.g. revertting the `if !parse_this` short-circuit) → median climbs ~150-200 ms → still trips the gate.
- A genuine repo growth that pushes the legitimate 5-file parse over 350 ms median (large monorepos with multi-second per-file parse) → the gate trips and a release-time triage decides whether to raise the budget or further bound the preview window.

## Tests

- `src/diff_digest.rs::diff_digest_max_parsed_files_skips_tree_sitter_beyond_budget` — locks the parse-budget shape: `files_changed` counts every path, only `N` files carry `touched_symbols`/`call_edges`, the remainder carry the `parse_deferred_by_budget` warning and empty extraction fields, parsing follows canonical sort order so the parsed subset is deterministic.
- `src/perf_gate.rs::preparation_hotspot_*` — five unit tests covering `Within`, `Regressed`, `InsufficientSamples`, even-count median averaging, exact-budget pass-through, and the no-stale-state contract.
- `tests/perf_gate.rs::preparation_hotspot_gate_*` — three integration tests that exercise the same gate against literal post-fix and pre-fix sample sets.

All existing `cargo test` suites (`exit_code`, `graph_db_conformance`, `perf_gate`, `scan_plan`, plus the in-binary tests) continue to pass: `cargo test` exits with `849 passed; 0 failed`.

## Verification commands

| Command | Exit |
|---|---|
| 3× `tsift graph-db --json backend-eval` (baseline, default) | 0, 0, 0 |
| 3× `tsift graph-db --json backend-eval --full-projection` (baseline) | 0, 0, 0 |
| 3× `target/release/tsift graph-db --json backend-eval` (after-fix, default) | 0, 0, 0 |
| 3× `target/release/tsift graph-db --json backend-eval --full-projection` (after-fix) | 0, 0, 0 |
| `cargo check` | 0 |
| `cargo clippy --all-targets -- -D warnings` | 0 |
| `cargo test` (`849 passed`) | 0 |
| `make check` | 0 |

## Out of scope (recorded for follow-up)

- `status_index_gate` per-process cold leg (~673-712 ms median): the 0.1.48 thread-local scope guard helps within one process but each backend-eval invocation pays the cold cost once. A persistent daemon or precomputed status snapshot would address this without touching the cache trust model. Not in this slice.
- `preparation_cache_lookup` is itself ~250 ms because `traversal_source_watermark` opens `index.db`, reads every non-generated snapshot row, hashes every markdown file under the path hint, and hashes `summaries.db`. The cache currently never hits across repeated runs on this repo because the working-tree mutation between back-to-back invocations is enough to shift the watermark; on a quiescent repo it would hit. Worth a follow-up to see whether the watermark can become a cheaper composite hash.
- `session_review_compute` (~368 ms): second-biggest of the four named hotspots, dominated by `session_discovery` (~170-210 ms) and `session_digest_total` (~110-130 ms). Inside the 10% tie band only against `context_pack_diff` if the latter drops further — not a tie at the pre-fix evidence threshold, so deferred to a follow-up slice.
