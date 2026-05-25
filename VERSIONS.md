# Versions

tsift is private software, but its command surface is still versioned explicitly.

Canonical binary version source: `Cargo.toml` `package.version`. The CLI exposes the same value via `tsift --version`.

Use `BREAKING CHANGE:` prefix in version entries to flag incompatible changes.

## Unreleased

## 0.1.49

- `#gdbprephot`: `conflict_matrix_preparation.context_pack_diff` is the dominant remaining hotspot in the four-named set (~445 ms default / ~481 ms full-projection median on agent-loop, vs `session_review_compute` ~368/370 ms, `impact` ~1.5 ms). `diff_digest::compute` now honors a new `max_parsed_files: Option<usize>` option that bounds per-file tree-sitter parsing, `git show HEAD:path` snapshot loads, and summary-cache lookups to the first `N` files in canonical sort order; files beyond the budget become path-only `DiffDigestFile` entries with a `parse_deferred_by_budget` warning. `build_context_pack_report_with_profile` wires this to `budget.preview_items()` (5 by default) since the preview only takes that many files anyway. Three-sample medians on agent-loop drop `context_pack_diff` from 445 ms to 289 ms default (-35%) and from 481 ms to (see plans/gdbprephot-evidence.md). Aggregate `symbols_touched`, `call_edges_added`, and `call_edges_removed` now reflect only the parsed subset for context-pack preview consumers; full-fidelity counts remain available via direct `tsift diff-digest` invocations which keep `max_parsed_files: None`. New `perf_gate::evaluate_preparation_hotspot` plus `CONTEXT_PACK_DIFF_BUDGET_MICROS = 350_000` lock the post-fix ceiling: callers MUST hand it freshly-acquired samples (no cached prior-run values) and the gate fail-closes below 3 samples to satisfy the "do not trust stale ownership" constraint. Five new unit tests (`diff_digest_max_parsed_files_skips_tree_sitter_beyond_budget`, `preparation_hotspot_*`) plus three new integration tests in `tests/perf_gate.rs` lock both the parse-budget contract and the gate verdict directions.

## 0.1.48

- `IndexDb::inspect_read_only` consults a thread-local `InspectScopeGuard` cache so a single trusted pipeline (e.g., `build_context_pack_report_with_profile` → `context_pack_status_reminders` → `status::check_status`) inspects the same `(root, .tsift/index.db)` exactly once instead of twice. Search and every other top-level call site runs outside any guard and gets identical fresh-per-call behavior — the regression test `search_timeout_reports_reindex_when_index_turns_stale_during_worker_run` still passes. `prepare_agent_doc_index_gate` invalidates the scope cache after a successful refresh so post-refresh status reflects the new DB. Three-sample medians on agent-loop show `status_index_gate` cold-leg drops from ~324 ms to ~53 ms (~271 ms reduction, over the 200 ms target for `#gdbgatecold`) and warm `context_pack_status_reminders` drops ~400 µs per call. Two new tests (`build_context_pack_reuses_inspect_within_scope`, `inspect_read_only_outside_scope_does_not_cache`) lock the scoped-cache contract.

## 0.1.47

- Adopt tagpath's `.naming/index.json` as a stable symbol-graph adapter (`#p6tsi`). New module `src/tagpath_adapter.rs` (`try_load`, `TagpathAdapter`, `LoadResult`, `HandleResolution`) wraps `tagpath::index` and is used by `tsift search` to annotate each `SymbolHit` with a stable `mem:<sha256[0..16]>` `tagpath_handle` when a fresh tagpath index is present at the project root. New search flags `--no-tagpath` (skip lookup) and `--tagpath-strict` (fail closed on a stale index). Stale indexes fall back to live extraction with a `tagpath_index_stale: true` stderr diagnostic. Existing users without `.naming/index.json` see no behavior change. Bumps the local `tagpath` path dep to 0.17.1 (with a slim `lang-rust,lang-python,lang-javascript,lang-typescript` feature set) and the workspace `tree-sitter` requirement to `^0.26`.

## 0.1.46

- `impact::compute` now exposes sub-phase timers under `conflict_matrix_preparation.impact.{context_resolution, diff_digest, test_path_scan, index_open, call_edge_impacts, route_handler_impacts, import_impacts, report_assembly}` and short-circuits the three iteration phases (`add_call_edge_impacts`, `add_route_handler_impacts`, `add_import_impacts`) when their inputs are empty. Three-sample medians on agent-loop show `conflict_matrix_preparation.impact` drops from 789 ms to 1.6 ms (-100%) on the typical backend-eval cold path (no staged changes), and the parent `conflict_matrix_preparation` drops from 2572 ms to 1935 ms (-25%). When staged changes are present the iteration phases run as before. The new sub-phases surface as `0us` on cache-hit reports with the existing source/document/staged-diff watermark detail, and the conformance suite asserts the sub-phases exist on cold runs.

## 0.1.45

- `status_index_gate` is decomposed into three sub-phases reported as `conflict_matrix_preparation.status_index_gate.{prepare_agent_doc_index_gate, context_pack_status_reminders, load_tag_ontology_preview_context}`. Three-sample medians on agent-loop confirm `prepare_agent_doc_index_gate` (422 ms, 62%) and `context_pack_status_reminders` (266 ms, 39%) split the cost; ontology loading is effectively free. New `prepare_agent_doc_index_gate_cached` wraps the gate behind an in-process `(root, path_hint, scope, packet_label)` cache so repeated invocations within the same process — daemon use cases, tests, traversal+context-pack pipelines — reuse the inspection result. Single-shot CLI flows do not benefit yet because each helper currently fires only once per `backend-eval` pipeline; the cold-path inspection cost stays owned by future work. Cache-hit reports surface the new sub-phases as `0us` with the source/document/staged-diff watermark detail. Conformance suite asserts the sub-phases exist on cold runs.

## 0.1.44

- `session_review` discovery now stat-walks Claude JSONL and Codex JSONL directories and only reads content for at most `MAX_RECENT_CANDIDATES_PER_SOURCE=64` newest files per source, and the per-file read is header-gated: a `BufReader` extracts the harness-specific `cwd` from the first 256 KB so files whose cwd does not match the target are skipped before any full read. Measured against agent-loop's ~2 GB / 2323-file Codex history and ~1.5 K Claude sessions, `conflict_matrix_preparation.session_review_compute.session_discovery` median drops from 3562 ms to 154 ms (-96%), `session_review_compute` parent drops from 3719 ms to 272 ms (-93%), and `conflict_matrix_preparation` overall drops from 5888 ms to 2148 ms (-64%), measured with `tsift graph-db --json backend-eval` three-sample medians on agent-loop using the new 0.1.43 sub-phase timers.

## 0.1.43

- `session_review_compute` now reports `target_context_build`, `session_discovery`, `session_digest_total`, `session_cost_total`, `session_aggregation`, and `report_assembly` sub-phases under `conflict_matrix_preparation.session_review_compute.<sub>` so the dominant preparation hotspot can be resolved at sub-phase granularity instead of as one 3.3–4.6 s opaque cost. Cache-hit reports also surface the same sub-phases as `0us` skipped with the source/document/staged-diff watermark guard, and the graph-db conformance suite asserts the sub-phases exist on cold runs and stay within 50 ms instrumentation slack of the parent phase.

## 0.1.42

- Conflict-matrix cache hits now report session-review, status/index gate, context-pack diff, exploration, graph orchestration, staged-diff, and impact phases as skipped 0us reuse guarded by source/document/staged-diff watermarks; backend-eval also requires real 128/256/512-hop metrics before any higher path cap can be considered, and all read-only prototype backends stay on hold until a native production adapter proves projection writes/load, parity, install, and lock behavior.
- Traversal source watermarks now exclude `.agent-doc` runtime markdown snapshots/baselines, preventing backend-eval full-projection and conflict-matrix cache keys from being invalidated by agent-doc closeout artifacts.
- `graph-db backend-eval` now has an opt-in `--full-projection` dataset, reports 128/256-hop path-tier probes alongside the 64-hop default and one-hop direct probes, and keeps FalkorDB on a hold decision until a production adapter beats SQLite across full-projection conflict-matrix/evidence/dispatch-trace/path/install/lock gates.
- `conflict-matrix` preparation now exposes split timings for cache lookup, session-review compute, status/index gate, context-pack diff, exploration materialization, graph orchestration, staged diff, and impact, plus source/document/staged-diff keyed `.tsift/conflict-matrix-cache` reuse for prepared context, staged-diff, impact, evidence, and target-scoped graph packets across CLI invocations.
- Normal `graph-db refresh`, `conflict-matrix`, and `dispatch-trace` now reuse the same source-watermark cached projection path as backend-eval when source inputs are unchanged, and conflict/trace preparation builds target-scoped graph snapshots instead of loading every graph node and edge.
- GraphStore now exposes cheap count and sample-edge probes; SQLite backs them with `COUNT(*)` / indexed `LIMIT 1` queries so backend-eval status and `path_max_hops` selection avoid full row materialization before timing the measured operation.
- Graph refresh now streams materialized node-property rows into the staged SQLite projection while node rows are inserted, and context-pack exploration uses one batched SQLite projection transaction for `source_handle` / `worker_context` rows instead of per-row autocommit writes.
- SQLite graph DB schema v3 now maintains `graph_node_properties` rows so `graph-db kind` and `neighborhood --property KEY=VALUE` use an indexed materialized property table instead of JSON extraction scans; refresh/status/doctor expose compaction proof, and `graph-db compact` adds a guarded WAL checkpoint/VACUUM path with explicit Convex-reconciliation confirmation before tombstone pruning.
- `graph-db evidence` now batches reachable worker-context, source-handle, worker-result, and semantic row expansion through one SQLite recursive CTE per target, preserving max-hop/limit ordering while avoiding per-family path walks.
- `conflict-matrix`, `dispatch-trace`, and `graph-db backend-eval` now reuse one prepared graph orchestration bundle per target set, including evidence packets, source-handle/worker-result/semantic expansion, graph snapshots, and dispatch-trace inputs, instead of repeatedly rewalking the same rows; repeated CLI calls can load the bundle from `.tsift/conflict-matrix-cache` when source/document/staged-diff and graph-freshness watermarks match.
- `graph-db backend-eval` now measures real, synthetic high-degree, and synthetic deep-chain datasets, emits metric-digest-ready raw and per-1k-graph-row normalized metrics plus replay/repeated-sample commands, and includes `fixtures/graph-db-performance-history.json` for repeatable performance-history comparisons.
- Added `tsift dependency-dag --path <session> [target...] --json` with a `dependency-dag-v1` contract for agent-doc backlog nodes, explicit dependency text, shared file/symbol/test/config and semantic overlap edges, worker-result follow-up edges, deterministic topological batches, cycle diagnostics, replay commands, and repair commands.
- `conflict-matrix` candidates and `worker_prompt_packets` now expose `previously_completed`; completed worker_result evidence downgrades missing source ownership to an informational warning instead of `per_target_fail_closed`, preventing completed agent-doc queue items from being reactivated only to rediscover ownership.
- Graph orchestration JSON surfaces now publish explicit contract versions and replay metadata: `graph-db evidence` emits `packet_id`, `projection_hash`, explicit worker/semantic result arrays, `replay_commands`, and `repair_commands`; `conflict-matrix`, `worker_prompt_packets`, `context-pack graph_orchestration`, `session-review --next-context`, and `dispatch-trace` carry matching contract markers for agent-doc consumers.
- Completed/blocked agent-doc worker responses now materialize as `worker_result` graph rows linked to backlog/job/source handles with status, touched files, expected tests, and follow-up ids, and `conflict-matrix` summarizes them as worker feedback with repeated-blockage warnings that do not weaken hard conflict gates.
- Added `tsift dispatch-trace --format json|html` for compact graph-backed operator review views linking backlog, job_packet, worker_result, source_handle, semantic rows, evidence packet ids, worker feedback, and worker_prompt_packets.
- Semantic dispatch ranking now includes fixture-covered score explanations while keeping file/symbol/test/config overlap as the hard fail-closed gate.
- Added `fixtures/graph-db-operator-examples/graph-orchestration-contracts.json` plus end-to-end refresh/status/doctor/evidence/stale-Convex-drift/convex-sync/conflict-matrix/context-pack/session-review operator commands for graph-backed dispatch.
- `conflict-matrix` now emits first-class `worker_prompt_packets` with owned files/symbols, read-only context, forbidden files, expected tests, expansion commands, and token budgets; target-specific source ownership prevents unrelated workers from inheriting every visible source window.
- Graph orchestration observability now carries projection freshness, evidence packet ids, conflict-matrix decisions, ownership block labels, and follow-up graph commands through `conflict-matrix`, `context-pack`, and `session-review --next-context`.
- `graph-db evidence` now includes reachable semantic concept/entity rows, and `conflict-matrix` uses those semantic rows as a ranking signal without overriding file/symbol/test/config conflict gates.
- Release verification now runs `cargo publish --locked --dry-run`, and the release docs/tests lock the `TSIFT_ENABLE_CRATES_PUBLISH` variable plus `CARGO_REGISTRY_TOKEN` secret contract for tagged crates.io publishes.
- Added an opt-in live Convex graph backend acceptance harness that applies a temporary projection to a dedicated deployment, pulls the remote snapshot, and runs graph-db node/kind/neighborhood/path parity against SQLite.
- Added `tsift graph-db doctor` for read-only SQLite `graph.db` and Convex snapshot diagnostics, including stale projection metadata, schema drift, orphan edges, duplicate ids, missing Convex index metadata, repair commands, and fail-closed exit codes.
- `tsift graph-db kind` and `graph-db neighborhood` now support deterministic node-id cursor pagination, repeatable `--property KEY=VALUE` node filters, page diagnostics, and backend parity coverage across SQLite and Convex snapshot stores.
- Added `examples/convex-graph`, a reusable Convex app-side schema/mutation/HTTP-action package for `tsift convex-sync --remote-snapshot --apply`, plus a local HTTP smoke test proving apply chunks round-trip through the documented transport shape.
- Agent-doc queue entries now materialize as `job_packet` graph nodes, and `context-pack` exploration packets now include bounded `worker_context` nodes linked to source handles so worker handoffs preserve prompt scope in the graph substrate.
- Added `fixtures/graph-db-operator-examples` with SQLite graph-db commands, Convex sync/apply examples, a stale snapshot fixture, and handle-reuse guidance for `traverse` / `context-pack`.
- `tsift traverse` and `context-pack` now materialize provider-neutral graph rows into `.tsift/graph.db` before report generation, including projection metadata/freshness, source-handle nodes, and Convex snapshot fail-closed validation; `tsift convex-sync` emits dry-run Convex `nodes`/`edges` upsert, tombstone, chunk, index, and freshness diagnostics.
- Added `tsift traverse`, a Graphify-style traversal surface that exports JSON/HTML graph slices with stable `gfil-*`, `gsym-*`, `gses-*`, and `gbak-*` handles for files, symbols, agent-doc sessions, and backlog items, plus neighborhood, shortest-path, and next-node recommendation reports for bug-fix navigation.
- `tsift status` now emits structured stale-index reminders, and `context-pack` carries the same reminders forward so agent handoff packs still show the reindex command and missing-summary follow-up when the repo index is stale.
- `log-digest` no longer reports clean `quit_after_eof` / user Ctrl-D exits as restart-churn warning signals; those exits remain summarized in runtime churn context while actual fresh restarts, timeouts, and Ctrl-D restart loops continue to warn.
- `session-review --next-context` now carries aggregate guardrails forward as `guardrail:<kind>` unresolved-failure action rows, so restart-loop, prompt-budget, cached-resend, and no-op closeout warnings remain visible in resumable handoff context even when no command failure was extracted.
- `log-digest` now classifies agent-doc runtime failures, restart churn, timeouts, and closeout churn as warning/error signals, so agent-doc logs no longer report `signal_groups: 0` while `session-digest` sees runtime failures and churn.
- `session-cost` no longer emits `restart_loop` guardrails from `max_restart_count` alone; restart-loop warnings now require actual restart-churn families such as fresh restarts, auto-trigger timeouts, or ctrl-d restart loops, with max restart count kept as contextual detail.
- Codex JSONL `session-digest` file-reference extraction now rejects shell redirection fragments and slash-separated conversational labels such as `agent-doc/tsift`, `digest/session`, `progress/CI-status`, and `version/preflight` unless they resolve to real files or carry recognized file names/extensions.
- `tsift rewrite` now leaves file-listing commands such as `rg --files`, `rg --type-list`, and `find ...` on the no-rewrite passthrough path so listing roots and predicates are not misread as exact search patterns.
- `session-digest` and `session-review` now filter assistant progress and assessment prose about failure-classification false positives, unresolved failure groups, and red/check CI-status commentary instead of reporting those meta lines as failures.
- `token-savings` fixtures now support source-read rewrite rows with required line-anchor validation, and the real-session fixture covers full-file `cat`/`bat` plus oversized `sed`/`head`/`tail` reads under a fail-under threshold.
- `session-cost` now reports repeated source-file read diagnostics for Claude/Codex transcripts, grouping native `Read` and common shell reads by path/range with duplicate-token estimates and concrete `tsift source-read` / `tsift summarize --file` follow-ups; `session-review` aggregates the same diagnostics across matched sessions.
- `log-digest` and `session-digest` now filter agent-doc runtime path fields that normalize to empty display paths or existing directories, preventing project-root `cwd_resolved` events from polluting file anchors and next-context file lists.
- Added deterministic lock-contention regressions for direct `index`, `search`, and `status` paths when SQLite WAL/SHM sidecars are live without a tsift-owned `index.lock`, preserving WAL-aware snapshot fallback and recovery guidance instead of raw lock errors.
- `session-cost` now prefers Codex `last_token_usage` records when cumulative `total_token_usage` streams interleave in one rollout, while still skipping duplicate cumulative snapshots and preserving the cumulative-delta fallback for older transcripts; `session-review` inherits the corrected totals and largest-turn outliers.
- `session-review` now aggregates token, command, failure, guardrail, and loop-cluster totals over the same bounded newest matched session rows it emits, and reports the newest matched session in a separate `latest_session_cost` scope so cached multi-session totals cannot be mistaken for active-session cost.
- `session-digest` and `session-review` failure rows now carry parsed command/session anchors, filter active prompt directives and source snippets out of failure extraction, and preserve real assertion/panic evidence plus named command exits such as `cargo test exited with code 1`.
- `tsift search` now delegates free-text query normalization to the `tagpath` v0.6.2 query API, while search/explain/session-review/context-pack preview refs derive canonical `tag_alias` values from the shared `tagpath` family API instead of local parser helpers.
- `context-pack` now loads tagpath ontology docs from `.naming/tags/*.md` and attaches compact ontology references to visible symbol refs, summary refs, and the top-level handoff payload so stable tag docs can be referenced by handle/path without repeating ontology prose.
- Regression coverage now locks ontology refs in both preview-builder unit tests and the compiled `context-pack --json` integration path.
- Budgeted `session-review --next-context` now keeps follow-up digest commands on an independent 4-command floor and preserves them verbatim, so small previews do not hide or corrupt the resume commands measured by the real-session token-savings fixture.
- Added a tsift-local deterministic SimWorld for session prompt extraction, rewrite routing, and status recommendation edge coverage. The fast corpus runs in normal `cargo test`; the wider ignored corpus runs in GitHub Actions through `make ci-full`.
- The generated Code Navigation guidance now tells agents to run local `make check`, then inspect the latest GitHub Actions CI run with `gh run list --workflow CI --limit 1` and fix red CI before calling work complete.
- `tsift log-digest` now treats structured agent-doc runtime fields as anchors: `file=...` and `path=...` become file refs without requiring line numbers, while timestamped event names plus `event=...`, `pane=...`, and `session=...` become structured symbol refs.
- Added a compiled-CLI regression proving `tsift status --fix --json` refreshes stale Code Navigation instructions from the prior binary version and reports the upgraded instructions as current.

## 0.1.42

- Added `tsift status --fix`, which applies safe local status recommendations before reporting: refreshes stale/missing root indexes, refreshes existing workspace scoped indexes when stale, and updates stale/missing Code Navigation instructions through the existing `tsift init` path.
- The injected Code Navigation instructions now tell agents to run `tsift status --fix` before relying on stale/missing tsift results when writes are allowed, or ask the user to run the printed `run:` command when writes are not allowed.
- Regression coverage now locks `status --fix` in both the in-process status command and the compiled CLI JSON path.

## 0.1.41

- Added `--budget <small|normal|deep|auto>` to the agent-facing preview surfaces for `search`, `explain`, `session-review`, and `context-pack`.
- `tsift --envelope` now applies the adaptive budget by default when callers do not pass explicit caps, with `auto` reading `TSIFT_CONTEXT_WINDOW`, `CODEX_CONTEXT_WINDOW`, or `CLAUDE_CONTEXT_WINDOW` to select small/normal/deep defaults.
- `tsift rewrite` now emits `tsift --envelope search ... --exact --budget normal` for `rg` and recursive `grep` rewrites, keeping hook output compact while avoiding hard-coded numeric caps in the command surface.

## 0.1.40

- `tsift --envelope __digest-runner ...` now probes `rtk rewrite` when RTK is installed, executes supported generic command families through RTK's compact filters, and records the chosen filter under `report.filter` while preserving the original command, exit code, digest payload, and artifact-backed transcript.
- The digest-runner envelope summary now includes a `filter` metric, so harnesses can see whether a build/test/log surface was compressed by RTK or by tsift's built-in digest path alone.
- Regression coverage now locks the RTK delegation path with a fake `rtk` binary, including envelope metadata and persisted filtered artifact content.

## 0.1.39

- `tsift rewrite` now makes token-saving agent surfaces automatic: `rg` / recursive `grep` rewrites produce `tsift --envelope search ... --max-items 5 --max-bytes 160`, and cargo/pytest/build rewrites produce artifact-backed `tsift --envelope __digest-runner ...` commands by default.
- `tsift init` now injects envelope-first Code Navigation guidance for `search`, `explain`, `session-review`, `context-pack`, and digest-runner test/build artifacts, so Codex and other non-Claude harnesses get the same bounded workflow through `tsift rewrite --run '<command>'`.
- Regression coverage now locks the default rewrite shapes plus end-to-end `rewrite --run` envelope execution without requiring callers to pass a global `--envelope` flag.
- `tsift rewrite` now forwards and deduplicates global structured-output flags into rewritten tsift commands, so callers can layer `--pretty`, `--terse`, or `--schema` onto the default summary-first `digest-runner` envelope.
- Regression coverage now locks the forwarded rewrite shape for `cargo install` plus end-to-end `rewrite --run` envelope execution for real `cargo test` and `cargo build` commands on a temp crate.
- `tsift --envelope __digest-runner ... --json` now returns a summary-first command/test-run envelope with command metadata, exit status, the existing bounded `test-digest` or `log-digest` payload under `report.digest`, and a persisted transcript artifact reference under `report.artifact`.
- Captured runner/build output is now written to `.tsift/artifacts/` with a stable handle plus a concrete replay command (`expand`) so green runs can stay terse in context while still offering an opt-in path back to the bounded digest.
- `tsift rewrite --run` now disables the default timeout when it is executing an already-tsift `search` command that did not specify `--timeout`, so capped exact-search passthroughs do not fail spuriously on broader scans.
- Regression coverage now exercises the new digest-runner envelope end to end, including persisted artifact creation for a passing test run.
- Added a global `tsift --envelope` wrapper for the bounded agent-facing `search`, `explain`, `session-review`, and `context-pack` JSON surfaces. The envelope carries a terse cross-command `tool`/`view`/`summary`/`follow_up` header while preserving the existing command-specific payload under `report`.
- Preview and handoff commands now expose one consistent machine-readable summary layer plus concrete follow-up commands, so MCP or CLI clients can render terse summaries and trigger narrower expansions without depending on prose formatting.
- Regression coverage now locks the new flag in CLI parsing tests and exercises the wrapped `context-pack` JSON output end-to-end.
- Added `tsift context-pack`, a single agent-facing handoff command that composes `session-review --next-context`, `diff-digest`, and optional `test-digest` / `log-digest` inputs into one bounded payload with resume commands.
- `context-pack` is bounded by default and accepts `--max-items` / `--max-bytes` so callers can keep resumable context packs stable under token pressure without replaying raw transcripts, diffs, or verbose logs.
- Regression coverage now locks the new command surface in CLI parsing, preview-builder unit tests, and a compiled end-to-end integration test that exercises the composed JSON payload.

## 0.1.38

- `tsift search --autoindex` now degrades instead of failing when a live tsift `index.lock` holder is already refreshing the target index: stale indexes continue through a read-only search path, and missing indexes fall back to exact live-file search until the writer finishes.
- The degraded success path emits one concise retry hint on stderr so callers know why symbol hits may lag or why exact search was used, without requiring a separate `tsift locks` run.
- Regression coverage now locks both the in-process and compiled CLI behavior for stale-index read-only fallback plus missing-index exact fallback under a live writer lock, while keeping rollback-journal lock failures fail-closed.

## 0.1.37

- `tsift rewrite` now supports `--run`, which executes the rewritten digest-first tsift command directly instead of only printing it for Claude hook integration.
- `rewrite --run` preserves the rewritten command's exit status and applies tsift-owned output caps for verbose human-readable `search`, `explain`, `graph`, `communities`, and `index` output, so Codex and other harnesses can stay bounded without depending on Claude `PreToolUse` hooks or RTK.
- Updated the injected Code Navigation guidance, spec, and harness-facing docs to point non-Claude harnesses at `tsift rewrite --run '<command>'` as the manual bounded fallback.

## 0.1.36

- `tsift session-cost` now emits bounded `loop_clusters` summaries for repeated prompt bodies, repeated command bundles, and repeated closeout churn across Claude JSONL, Codex JSONL, and `agent-doc` runtime logs.
- `tsift session-review` now aggregates those loop clusters across matched sessions, so repeated verification bundles and no-op closeout churn become explicit review signals instead of hiding inside broad command/runtime totals.
- Regression coverage now locks the new loop-cluster surface in direct/unit tests and compiled CLI integration tests for both `session-cost` and `session-review`.

## 0.1.35

- `tsift session-review` now learns historical document path aliases plus prior `session=` aliases from the matching `agent-doc` runtime log before it scans Claude/Codex transcripts, so renamed task files and migrated session ids still collapse into one comparable review.
- File-target session matching no longer relies on filename-only aliases or arbitrary raw transcript substrings. Claude/Codex candidates now match only against structured user/tool-input snippets, which prevents unrelated hook output or command stdout from pulling in the wrong session history.
- Claude/Codex transcript parsing for `session-review` now skips malformed JSONL lines instead of failing the whole review, and Claude non-conversation attachment records are ignored without noisy warnings so cross-harness results stay comparable.

## 0.1.34

- `tsift session-review` now includes a bounded `next_context` payload in its JSON report and supports `--next-context` to emit only the resumable handoff pack for a document or repo target.
- The new next-context pack carries only active prompt targets, the latest verification closeout state, touched files/symbols, unresolved failure hotspots, and the next digest commands to use instead of replaying raw session/transcript/log context.
- Regression coverage now locks the new next-context surface in direct/unit tests, CLI parsing tests, and compiled CLI integration tests for both the full JSON review and the dedicated `--next-context` output.

## 0.1.33

- Added `tsift session-review`, a cross-harness aggregate review for a document or repo path. It auto-discovers related Claude JSONL, Codex JSONL, and `agent-doc` runtime logs, then emits one bounded combined digest + cost report instead of requiring manual per-log review.
- `session-review` reuses the existing `session-digest` and `session-cost` parsers, aggregates their bounded signals into one report, and matches document sessions by cwd/path plus `agent_doc_session` log aliases when available.
- File-target `session-review` matching now fails closed on shared-workspace cwd hits: Claude/Codex transcripts must also mention the target document path or `agent_doc_session` before they count as a matched session, while directory targets still use cwd matching.
- Regression coverage now locks the new command in direct/unit discovery tests, CLI parsing tests, and a compiled CLI integration test that exercises Claude/Codex/agent-doc auto-discovery through `HOME`.
- `tsift session-cost` and `tsift session-digest` now derive bounded restart-churn families from `agent-doc` runtime logs, so `fresh_restart`, `auto_trigger_timeout`, ctrl-d restart loops, and quit-after-eof cycles are summarized directly instead of being buried in raw event counters.
- Regression coverage now locks the new restart-churn summaries in both direct/unit tests and compiled CLI stdin tests for `session-cost` and `session-digest`.
- `tsift init` now injects owning-root guidance into the Code Navigation section so harnesses switch to the relevant repo or submodule root before tsift/build/test work instead of accidentally carrying the superproject instruction surface into submodule tasks.
- The injected Code Navigation section now also steers Claude/Codex toward `session-digest`, `session-review`, `diff-digest`, `test-digest`, and `log-digest` instead of raw transcript replays, `git diff/show/log` patch dumps, or verbose build/test output reads.
- Harness-oriented digests (`session-digest`, `log-digest`, `test-digest`) now prefer the nearest owning git root over the outer workspace `.gitmodules` root, so transcript reads and digest enrichment stay scoped to `src/tsift` when the source file lives there.
- `tsift rewrite` now anchors long transcript/log reads to that owning repo or submodule root before routing them into `session-digest`, and regression coverage now locks the new root-selection behavior in both direct/unit and compiled CLI rewrite tests.
- `tsift session-digest` now supports Codex JSONL and `agent-doc` runtime `.log` inputs in addition to markdown session docs and Claude JSONL, so bounded session evidence no longer depends on replaying raw harness transcripts or restart logs.
- `tsift rewrite` now recognizes long Codex JSONL reads and `agent-doc` runtime log reads and routes them to `tsift session-digest` instead of spilling raw session/log content into agent context.
- Regression coverage now locks the new session-digest parser paths and rewrite detection in both direct/unit tests and compiled CLI integration tests.
- Added `tsift session-cost`, a bounded token/runtime-cost digest for Claude JSONL, Codex JSONL, and `agent-doc` runtime logs. It reports prompt totals, cached-input ratios, output totals, largest turn outliers, and restart-churn counters without replaying the raw session.
- `session-cost` normalizes Claude cache-read/cache-create usage and Codex cumulative `token_count` events into one report, dedupes repeated Claude assistant message ids, and skips duplicate Codex cumulative snapshots so token totals stay stable.
- Regression coverage now locks the direct helper logic, CLI parsing, and compiled CLI stdin path for the new command.
- `tsift search` human-readable output now collapses repeated high-hit file matches into grouped file rows with hit counts before representative snippets, so broad exact/literal lookups stay usable without depending on RTK-only truncation.
- `tsift explain` now applies the same file-level grouping idea to dense caller/callee sets in its default human output, while leaving JSON and tabular outputs unchanged.
- Regression coverage now locks the grouped search/explain rendering in both direct/unit tests and compiled CLI integration tests.

## 0.1.32

- `tsift rewrite` now auto-routes long transcript reads for recognized agent-doc markdown sessions and Claude JSONL handoffs into `tsift session-digest` instead of spilling raw session history into agent context.
- The new transcript-read rewrite coverage is intentionally narrow: it only intercepts `cat`, `bat`, `head -n`, `tail -n`, and `sed -n` patterns when the target file looks like a real session transcript and the requested read is large enough to be token-expensive.
- Regression coverage now locks the new session-read rewrite behavior in both direct/unit tests and a compiled CLI rewrite integration test.

## 0.1.31

- `tsift diff-digest` now supports `--cached` for staged-index review and `--revision <rev>` for single-commit/history review, while keeping the existing working-tree mode as the default.
- `tsift rewrite` now auto-routes `git diff --cached`, `git show`, and simple patch-style `git log -p -1 ...` commands into the bounded diff-digest surface instead of letting raw non-working-tree hunks spill into agent context.
- Regression coverage now locks the new staged/revision digest behavior in both direct/unit tests and compiled CLI integration tests.

## 0.1.30

- Added `tsift session-digest`, a bounded transcript digest for markdown session docs and Claude JSONL. It extracts prompt targets, shell commands, touched files/symbols, failures, and closeout evidence such as verification/install/commit/push/version mentions.
- `session-digest` auto-detects markdown versus JSONL by default, supports explicit `--source markdown|jsonl`, and stays transcript-only instead of replaying tool calls or attempting full conversation reconstruction.
- Regression coverage now locks the direct helper logic, CLI parsing, and compiled CLI stdin path for the new command.

## 0.1.29

- Added `tsift metric-digest`, a generic metric-run digest for repeated benchmark/test/perf-style workflows. It reads JSON/NDJSON run history from stdin or `--input`, compares the latest run against a prior run or `--baseline`, and emits compact deltas plus markdown-ready history tables.
- `metric-digest` infers common metric directions (`mae`, `latency`, `cost`, `accuracy`, `score`, etc.), supports explicit `--metric`, `--lower-is-better`, and `--higher-is-better` overrides, and surfaces top improvements/regressions without hard-coding any session-share-specific schema.
- Regression coverage now locks the direct helper logic, CLI parsing, and compiled CLI stdin path for the new command.

## 0.1.28

- `tsift rewrite` now auto-routes plain `git diff` to `tsift diff-digest`, `cargo test` / `pytest` to a tsift-owned test-digest wrapper, and common verbose cargo build/check/clippy/install commands to a log-digest wrapper instead of leaving those high-token commands raw by default.
- The new hidden `tsift __digest-runner` helper executes the wrapped shell command, digests the captured stdout/stderr through `test-digest` or `log-digest`, and preserves the original exit code so failing tests/builds still fail closed.
- Regression coverage now locks the rewrite rules plus the digest-runner exit-code behavior in both unit tests and compiled CLI integration tests.

## 0.1.27

- Added `tsift log-digest`, a bounded verbose-log digest that reads captured stdout/stderr from stdin or `--input`, collapses repeated lines, groups warning/error signals, extracts file anchors and stack blocks, and emits JSON or compact human output.
- `log-digest` keeps summary enrichment read-only: when `.tsift/summaries.db` already has current rows for anchored files or extracted symbols, the digest includes up to two cached summary snippets; otherwise it degrades to `missing`, `stale`, or `unavailable` without mutating the cache.
- Regression coverage now locks this behavior in both the direct helper surface and the compiled CLI stdin path.

## 0.1.26

- Added `tsift test-digest`, a bounded test-output digest that reads captured runner output from stdin or `--input`, auto-detects `cargo`/`pytest` formats, groups duplicate failures, preserves file/line anchors, and emits JSON or compact human output.
- `test-digest` keeps summary enrichment read-only: when `.tsift/summaries.db` already has current rows for an anchored file, the digest includes up to two cached summary snippets; otherwise it degrades to `missing`, `stale`, or `unavailable` without mutating the cache.
- Regression coverage now locks this behavior in both the direct helper surface and the compiled CLI stdin path.

## 0.1.25

- Added `tsift diff-digest`, a bounded diff-adjacent report that compares `HEAD` to the working tree (plus untracked files) and emits changed files, touched symbols, current cached summary snippets when available, and added/removed call edges.
- `diff-digest` does not require a fresh `index.db`; it parses the changed file snapshots directly so unindexed working-tree edits still show up in the digest.
- Regression coverage now locks this behavior in both the direct helper surface and the compiled CLI command.

## 0.1.24

- Plain `tsift search <query>` now auto-promotes single-token identifier/path-like queries such as `claudescore-3`, `alpha_helper`, `src/main.rs`, and `crate::module` to the exact `rg -F` backend even when the caller does not spell `--exact`.
- That keeps the fast literal lookup path on by default for the query shapes that lexical BM25 tokenization handles worst, while still leaving plain word and multi-word prose searches on the lexical path.
- Native content/FTS indexing remains deferred for now because the main remaining lookup gap was backend selection, not missing indexed content storage.
- Regression coverage now locks this behavior in both the direct command path and the compiled CLI search surface.

## 0.1.23

- `tsift search --exact` now routes literal lookups through a first-class `rg -F` backend instead of sending every rg-style query through lexical BM25, so identifier-like searches such as `claudescore-3` return direct file hits without paying sift corpus/BM25 startup cost.
- Exact searches bypass the lexical stale-index precheck and the workspace shared-root-index requirement, so they still work when `.tsift/index.db` is stale/missing or when a workspace only has scoped `.tsift/indexes/<scope>/index.db` files.
- The `tsift rewrite` hook now rewrites `rg ...` and `grep -r ...` commands to `tsift search --exact ...`, preserving the fast literal-search path instead of silently translating those commands into lexical search.
- Regression coverage now locks this behavior in the direct exact-search helpers, the CLI parser, and the rewrite surface.

## 0.1.22

- `tsift search` now routes both in-process lexical searches and the timed `__search-worker` helper through a stable `.tsift/search-cache` directory rooted at the resolved project/workspace root, so repeated searches can reuse sift corpus/BM25 artifacts instead of rebuilding them from scratch every run.
- Scoped and federated searches share that same root-owned cache location rather than creating ad hoc caches under nested paths, so workspace searches keep their reusable search state under the canonical `.tsift/` tree.
- Regression coverage now locks this behavior in both the direct search helpers and the compiled CLI search surface.
- `tsift search` timeout diagnostics now re-check the same index targets after a worker timeout. Fresh indexes stop getting the misleading "index may be stale" hint, while indexes that became stale or disappeared mid-search now get a concrete reindex command in the timeout error itself.
- Regression coverage now locks this behavior in both the direct timeout helper and the compiled CLI search surface.
- `tsift status` now derives its `tsift summarize --extract ...` follow-up from the indexed layout instead of hardcoding `src/`, so root-level repos recommend `.` and workspace layouts only keep `src/` when that is the real shared scope prefix.
- Regression coverage now locks this behavior in the direct status helpers for single-root, `src/`-rooted, and mixed workspace layouts.
- `tsift status` now auto-builds missing workspace scoped indexes before it prints the final report, so a workspace with checked-out submodules but absent `.tsift/indexes/<scope>/index.db` files can recover to a completed status in one command instead of stopping at `index: missing` / `stale`.
- That auto-repair path only fills the missing scoped indexes; the low-level `status::check_status` helper remains read-only and stale-file reporting still stays visible after the rebuild pass.
- Regression coverage now locks this behavior in both the direct command path and the compiled CLI `status --json` surface.
- Read-only `index.db` and `summaries.db` recovery is now WAL-aware end to end: when a live SQLite lock blocks reads and `-wal` / `-shm` sidecars are present, tsift copies that live sidecar state into the snapshot fallback instead of copying only the main `.db` file or waiting for a rollback-journal marker that never appears in normal WAL mode.
- `tsift status` / `tsift locks` now report WAL sidecar presence explicitly and distinguish `snapshot_fallback_wal` recovery from the older rollback-journal snapshot path, so lock diagnostics describe the real live lock mode instead of implying every fallback came from `*.db-journal`.
- Regression coverage now locks this behavior in the shared read-only helpers, the direct status/summary readers, and compiled CLI `status` plus `summarize --stats` flows under a live WAL writer.

## 0.1.21

- Plain `tsift search` on a workspace root no longer auto-creates `.tsift/index.db` when the workspace only has scoped `.tsift/indexes/<scope>/index.db` files. It now fails closed and requires the caller to pick `--scope <scope>` or `--federated`.
- The new workspace-search error lists both the available scope ids and the currently indexed scope ids, so agents can choose the right search target without guessing or mutating the workspace layout by accident.
- Regression coverage now locks this behavior in both the direct command path and the compiled CLI search surface.
- Read-only summary queries (`tsift summarize --stats`, `tsift summarize <symbol>`, `tsift summarize --file <path>`) now retry against a snapshot copy when a rollback-journal lock wedges the live `summaries.db`, instead of surfacing a raw `database is locked` failure.
- `tsift status` summary coverage checks now use that same resilient summary read path and expose `recovery: snapshot_fallback` / `summaries recovery: ...` diagnostics when they had to degrade off the live cache.
- Regression coverage now locks this behavior in the low-level summary reader, the direct summarize/status command paths, and the compiled CLI summarize surface.

## 0.1.20

- `tsift status` now treats workspace scoped indexes as authoritative whenever `.gitmodules` defines scopes, even if a shared `.tsift/index.db` also exists, so missing scoped DBs can no longer masquerade as a fresh workspace.
- Mixed root-plus-scoped workspace status now keeps reporting `workspace_scopes` and `missing_scopes`, and the top-level recommendation continues to point at `tsift index --workspace .` instead of the shared-root path.
- Regression coverage now locks this behavior in both the direct status helpers and the compiled CLI status surface.

## 0.1.19

- `tsift status`, `tsift search`, and the read-only graph query commands now resolve nested input paths against the nearest ancestor project root that already owns `.tsift/`, instead of treating subdirectories as brand-new projects.
- Nested-path query calls therefore reuse the existing root or scoped indexes and stop auto-creating stray subdirectory `.tsift/index.db` state during search autoindex.
- Regression coverage now locks this behavior in the shared path-resolution helper, the direct command paths, and the compiled CLI query/status surface.

## 0.1.18

- `tsift summarize --extract <path> --diff` now includes untracked files under the requested extract scope, instead of only re-extracting tracked paths reported by `git diff --name-only HEAD`.
- Diff extraction now skips deleted paths before the summarize walk, so `--diff` only feeds readable source files into the extraction batch.
- Regression coverage now locks this behavior in the direct summarize diff path and the compiled CLI summarize surface.

## 0.1.17

- `tsift graph`, `tsift communities`, `tsift path`, and `tsift explain` now fail closed on workspace roots that only have scoped `.tsift/indexes/<scope>/index.db` state, instead of pointing at a missing `.tsift/index.db` and hiding the real fix.
- The new error explicitly requires `--scope <scope>` and lists both the available scope ids and the currently indexed scopes, so agents can pick a valid workspace query target without guessing.
- Regression coverage now locks this behavior in both the direct command path and the compiled CLI query surface.

## 0.1.16

- `tsift status` no longer reports a partially indexed workspace as `fresh`. If some configured scoped `index.db` files are missing, full-workspace misses remain `index: missing` while partial workspaces surface as `index: stale` with explicit `missing_scopes`.
- Workspace status output and `--json` now list the missing scope ids directly, so agents can distinguish "files changed" from "this configured submodule has never been indexed yet."
- Regression coverage now locks this behavior in both the direct status helpers and the compiled CLI status surface.

## 0.1.15

- `tsift index --submodule <name>` now uses the same strict workspace scope resolution as `--scope`, so unknown selectors fail closed instead of indexing `root/<name>` into an unreachable scoped database.
- Ambiguous duplicate leaf-name selectors now fail closed for submodule indexing too, requiring the concrete scope id when `.gitmodules` contains colliding leaf names.
- Regression coverage now locks this behavior in both the direct `cmd_index` path and the compiled CLI index surface.

## 0.1.14

- `tsift status` now detects workspace-only indexes under `.tsift/indexes/<scope>/index.db` instead of reporting `index: missing` whenever the root `.tsift/index.db` is absent.
- Workspace status output now reports the indexed scopes explicitly, aggregates their freshness into the top-level `index` state, and recommends `tsift index --workspace .` / `tsift init --workspace` for workspace roots.
- Regression coverage now locks this behavior in both the direct status helpers and the compiled CLI status surface.

## 0.1.13

- Workspace scope identifiers now stay unique even when `.gitmodules` contains duplicate trailing directory names. Unique leaves still use the short leaf name (for example `alpha`), but duplicate leaves promote to the full submodule path (for example `pkg/app/foo`, `vendor/foo`) so indexing and scoped search no longer collide onto the same `index.db`.
- Ambiguous legacy leaf selectors now fail closed and list the concrete scope ids to use, instead of silently resolving to whichever duplicate scope happened to win first.
- Regression coverage now locks this behavior in config parsing, in-process workspace search, workspace indexing, and the compiled CLI search surface.

## 0.1.12

- Workspace `tsift summarize --extract ...` now resolves symbol context per extracted file, so files under `.tsift/indexes/<scope>/index.db` use the matching scoped index instead of whichever workspace index appears first.
- Summarize symbol preload now uses exact normalized file matches instead of suffix matching, preventing same-path collisions across scoped indexes and locking the prompt context to the intended file.
- Regression coverage now locks this behavior in the direct summarize helpers, the workspace summarize command path, and the compiled CLI summarize surface.

## 0.1.10

- `tsift summarize --stats`, `tsift summarize <symbol>`, and `tsift summarize --file <path>` now fail closed when `.tsift/summaries.db` is absent and otherwise open the summary cache read-only, so lookup paths no longer create or contend on the cache DB.
- Regression coverage now locks this behavior in both the direct `cmd_summarize` path and the compiled CLI summarize surface.

## 0.1.11

- `tsift summarize --extract <relative>` now resolves the walked extraction path against `--path` / the canonical project root instead of the caller's current working directory, so batch extraction targets the intended repo even when the CLI runs from elsewhere.
- Regression coverage now locks this behavior in both the helper-level summarize path resolution and the compiled CLI summarize surface.

## 0.1.9

- `tsift lint --index .tsift/indexes` now treats the scoped-index directory itself as a valid discovery root, so explicit per-submodule linting no longer ignores every `index.db`.
- Regression coverage now locks this behavior in both the helper-level entity discovery path and the compiled CLI lint surface.

## 0.1.8

- `tsift lint` now opens discovered `index.db` files through the shared read-only path with rollback-journal snapshot fallback, so markdown linting stays available while a live writer holds the database lock.
- Regression coverage now locks this behavior in both the helper-level entity-loading path and the compiled CLI lint surface.

## 0.1.7

- `tsift lint` now auto-discovers live `index.db` files from the nearest ancestor `.tsift` root, including scoped `.tsift/indexes/*/index.db` layouts, instead of probing the retired `symbols.db` paths.
- Regression coverage now locks this behavior in both the helper-level discovery path and the compiled CLI lint surface.

## 0.1.6

- `tsift search --scope <name>` now fails closed when the named submodule does not exist, and reports the available workspace scopes instead of silently falling back to a full-workspace lexical search.
- Regression coverage now locks this behavior in both the direct `cmd_search` path and the compiled CLI integration test surface.

## 0.1.5

- `tsift communities` now opens `index.db` through the same read-only path as `graph`, `path`, and `explain`, so it no longer acquires the `index.lock` writer sidecar for a read-only graph query.
- Regression coverage now holds a live writer lock and asserts that both the in-process command path and the compiled CLI still succeed for `tsift communities`.

## 0.1.4

- `tsift index --prune` now falls back to the same full file-mtime scan as normal incremental indexing, so file edits inside unchanged directories are still detected correctly.
- The `--prune` flag remains in place as a compatibility surface and reports prune stats, but active subtree skipping is suspended until tsift has a sound invalidation model that cannot miss in-place file edits.

## 0.1.3

- `tsift index` now records non-fatal warnings when a changed file cannot be read or when symbol/call extraction fails, instead of silently swallowing those `.ok()` paths.
- Those warnings are emitted on stderr from shared index-update flows and also carried in the structured `IndexSummary`, so manual indexing and search autoindex no longer hide partial extraction failures.

## 0.1.2

- Writable `index.db` opens now set and verify `PRAGMA wal_autocheckpoint=256`, so routine tsift writes checkpoint the WAL on an explicit budget instead of relying on SQLite defaults.
- Regression coverage now asserts the busy timeout, WAL journal mode, and explicit auto-checkpoint setting together.

## 0.1.1

- `tsift search --timeout` now runs the bounded sift search in an internal helper process and kills that worker on timeout, so timed-out searches no longer keep burning CPU in detached threads.
- `--timeout 0` still keeps search in-process for long-running sessions that explicitly opt out of the timeout.

## 0.1.0

- Initial private versioned release surface for the tsift CLI.
- Commands available: `index`, `search`, `graph`, `communities`, `path`, `explain`, `edit`, `route`, `rewrite`, `sql`, `audit`, `summarize`, `lint`, `status`, `init`.
- Global output controls available: `--compact`, `--pretty`, `--terse`, `--schema`, `--absolute`, `--tabular`.
- Project setup includes Code Navigation instruction injection plus optional Codex auto-reindex hook install via `tsift init --codex`.
- `tsift search` now fast-fails on stale existing indexes and adds `--autoindex` for hook-like one-off recovery in unhooked sessions.
- Writable index updates now use a sibling `index.lock` sidecar so concurrent `tsift index` / `tsift search --autoindex` writers fail fast with a tsift-owned lock message instead of raw SQLite lock errors.
- Instruction version markers: `tsift init` now embeds `v=X.Y.Z` in the `<!-- tsift:code-navigation -->` opening marker. `tsift status` reports `instructions: current|stale|missing` and recommends `tsift init` when the installed version differs from the marker version. Pre-versioned markers (no `v=` attribute) are treated as stale.
