# tsift Spec — Digests & Session Review

Part of the [tsift spec](../SPEC.md). See that index for the full command/spec map.

## Diff Digest

`tsift diff-digest [path]` turns worktree, staged, or single-revision diffs into a bounded, code-aware report for agent context.

```bash
tsift diff-digest .        # current repo root
tsift diff-digest --cached . # staged index against HEAD
tsift diff-digest --revision HEAD . # HEAD commit against its first parent
tsift diff-digest --json . # structured output
tsift diff-digest --max-parsed-files 0 . # unlimited tree-sitter parsing
```

Behavior:

1. In default mode, collect tracked changes from `HEAD` plus untracked files and compare `HEAD` to the working tree. With `--cached`, compare the staged index to `HEAD`. With `--revision <rev>`, compare that single revision to its first parent (or to the empty tree for a root commit).
2. Parse both snapshots directly with tree-sitter when the file language is supported. By default, only the first 25 changed files (in sort order) receive full tree-sitter parsing; remaining files get cheap path-only entries. `--max-parsed-files N` adjusts the cap; `--max-parsed-files 0` disables it.
3. Emit changed-file status, touched symbols, up to two current cached summary snippets when `summaries.db` matches the compared snapshot, and added/removed call edges.

`diff-digest` intentionally does not require a fresh `index.db`. It reads the compared snapshots directly so unindexed working-tree edits, staged-only content, and historical commit review all stay bounded without mutating the index. Summary lookups stay read-only and degrade to `missing`, `stale`, or `unavailable` instead of mutating the cache.

## Test Digest

`tsift test-digest` turns captured test runner output into a bounded failure report for agent context.

```bash
cargo test 2>&1 | tsift test-digest --path .
tsift test-digest --runner pytest --input .pytest-failures.log --json
```

Behavior:

1. Read captured test output from stdin by default, or from `--input <file>`.
2. Auto-detect `cargo` and `pytest` output formats unless `--runner` forces one parser.
3. Group duplicate failures by file/line/message, preserve the failing test names, and keep the first assertion/error message instead of the full transcript noise.
4. When `.tsift/summaries.db` already has current rows for an anchored file, include up to two cached summary snippets; otherwise report `missing`, `stale`, or `unavailable` without mutating the cache.

`test-digest` is intentionally transcript-only. It does not execute the test runner itself, and it keeps summary enrichment read-only so digesting noisy output never contends with `tsift summarize --extract`.

## Metric Digest

`tsift metric-digest` turns repeated metric-run histories into bounded deltas for agent context and news updates.

```bash
tsift metric-digest --input runs.json
tsift metric-digest --baseline yesterday.json --input today.json --metric session_mae --metric composite_score
cat benchmark-runs.ndjson | tsift metric-digest --lower-is-better session_mae --higher-is-better composite_score
```

Accepted input shapes:

- a single JSON object with a `metrics` map
- a JSON object with `runs: [...]`
- a JSON array of run objects
- NDJSON with one run object per line

Each run object may include `label`, `id`, and `timestamp`, plus either `metrics: {key: number}` or inline numeric metric fields.

Behavior:

1. Read run history from stdin by default, or from `--input <file>`.
2. Compare the latest input run against `--baseline <file>` when present; otherwise compare it against the previous run in the same history.
3. Infer common metric directions automatically (`mae`, `latency`, `cost`, `error` prefer lower; `score`, `accuracy`, `pass`, `throughput` prefer higher) and allow explicit `--lower-is-better` / `--higher-is-better` overrides.
4. Emit bounded per-metric deltas, top improvements/regressions, and a markdown-ready history table suitable for session notes or news updates.

`metric-digest` is intentionally schema-light. It does not execute the underlying benchmark/test/perf workflow, and it avoids hard-coding session-share-specific parsers so different run producers can feed the same digest surface.

When a run includes `communities.<workload>.*` or `community_search.<workload>.*` metrics, `metric-digest` also emits a `community_search_gate` report. The gate requires both `real` and `synthetic_multi_module` workloads and checks `duration_micros` or `runtime_micros`, `handle_coverage_pct`, `stale_behavior_pass`, `no_tagpath_behavior_pass`, `duplicate_name_precision`, and `top_community_stability`. Runtime is tracked as lower-is-better and blocks when it regresses by more than 25% against the compared run; handle coverage must stay at or above 95%, duplicate-name precision at or above 0.99, top-community stability at or above 0.95, and stale/no-tagpath behavior metrics must report `1`. The checked-in fixture `fixtures/community-search-gate-history.json` records the canonical real plus synthetic multi-module sample shape and can be inspected with `tsift metric-digest --input fixtures/community-search-gate-history.json --json`.

When a run includes `memgraphrag.<workload>.*` or `mem_graph_rag.<workload>.*` metrics, `metric-digest` also emits a `memgraphrag_performance_gate` report. The gate requires `memory_query`, `memory_project_graph`, `graph_db_related`, and `semantic_seeded_neighborhood` workloads and checks `duration_micros` (with `latency_micros` / `runtime_micros` accepted as aliases). Each workload must compare against a prior or `--baseline` run, and blocks when latency regresses by more than 25% or when the current/baseline latency metric is missing. The checked-in fixture `fixtures/memgraphrag-performance-history.json` records the canonical four-surface sample shape for memory query planning, memory graph projection, `graph-db related`, and semantic seeded neighborhood retrieval, and can be inspected with `tsift metric-digest --input fixtures/memgraphrag-performance-history.json --json`.

## DCI Benchmark

`tsift dci-benchmark --fixture <path>` summarizes recorded Direct Corpus Interaction search runs for multi-hop repo/code tasks. The benchmark fixture compares the three strategy lanes tsift cares about after the DCI paper review:

- `exact_chained_rg`: literal `rg -F` / `tsift search --exact` narrowing with local context expansion
- `lexical_bm25`: the default sift/BM25 search path
- `hybrid`: slower BM25 + vector-assisted search

Each task records whether the strategy localized the intended edit/review target plus `tool_calls`, `latency_ms`, and `estimated_tokens`. Recorded retrieval fixtures can also provide `expected_strategies`, `useful_hits`, `output_tokens`, and `zero_output` fields so non-code retrieval surfaces can compare useful result density, emitted result budget, and zero-output failure rate without changing the summarizer. The report aggregates localization rate, useful hits, zero-output rate, average tool calls, average latency, average token budget, and average output tokens per strategy, then ranks strategies by localization, useful hits, zero-output avoidance, and agent budget. Missing expected lanes are warnings, not hard failures, so partial experiments can still be digested while making gaps visible. When a fixture declares `claude_mem_api`, `tsift_session_review_context_pack`, and `graph_db_related`, the JSON report also emits a `memory_retrieval_gate` cutover decision: each tsift candidate must have at least the claude-mem average useful-hit rate and fewer zero-output failures than the claude-mem baseline, with equal zero failures accepted when the baseline already has zero.

The checked-in `fixtures/dci-search-benchmark.json` is a seed benchmark for tsift's own multi-hop workflows: rewrite/digest routing, summary-cache lock fallback, and workspace scope fail-closed localization. `fixtures/memory-retrieval-eval.json` is the memory-retrieval fixture derived from the Claude Code Insights report and sampled `claude-mem` failures; it compares `claude_mem_api`, `tsift_session_review_context_pack`, and `graph_db_related` on useful hits, output tokens, latency, and zero-output failures. These fixtures are intentionally recorded-run based rather than live runners, so CI stays deterministic and hybrid/vector model downloads do not gate normal verification. Live benchmark scripts can append new task records and use `tsift dci-benchmark --json` as the stable summarizer. Direct claude-mem reads stay in fallback/rollback mode until `memory_retrieval_gate.decision=pass`, `tsift memory status` shows full import and graph semantic retrieval readiness, and one normal session cycle completes without direct `claude-mem` or `/mem-search` reads.

## Deterministic SimWorld

`src/sim_world.rs` provides a tsift-local deterministic simulation harness for high-risk agent workflow states that should not require live tmux or long CLI matrices. The named trace, fast corpus, and wider medium corpus run in normal `cargo test`.

The model currently covers:

- session prompt-target extraction, including live exchange prompts versus copied instruction/frontmatter/archive ballast;
- rewrite routing for long session reads, large indexed source reads, short passthrough reads, test/build digest-runner wrappers, diff-digest routing, and shell metacharacter passthrough;
- status recommendation transitions for missing, stale, and current Code Navigation instructions.

Coverage counters are explicit and fail closed when a named edge class disappears from the corpus. This mirrors the agent-doc pattern of replacing expensive live tmux edge sweeps with deterministic model coverage first while keeping the deterministic simulation budget small enough for the local default suite.

## Log Digest

`tsift log-digest` turns captured verbose stdout/stderr into a bounded transcript digest for agent context.

```bash
cargo build 2>&1 | tsift log-digest --path .
tsift log-digest --input target/build.log --json
```

Behavior:

1. Read captured log output from stdin by default, or from `--input <file>`.
2. Collapse repeated lines, group warning/error signal lines, classify agent-doc runtime failures/restart loops/timeouts/closeout churn as signals, keep clean user quit-after-EOF exits out of restart-churn warnings, and count repeated stack blocks so noisy transcripts stay bounded.
3. Fold near-duplicate lines into semantic line families. Beyond exact-line collapsing, each line is reduced to a template by replacing its variable tokens — bracketed epoch timestamps, file paths, semantic versions, hex hashes/object ids, numbers/durations/sizes/percentages, the value side of `key=value` structured fields, and the cargo-style crate name in the canonical `<Verb> <crate> v<version>` three-token progress shape (so connectives such as `from`/`to` in lines like `Downgrading serde from v1.0.0` are not mistaken for the crate name and folded away) — with stable placeholders. Lines sharing a template are grouped into a family carrying its template, total occurrence count, distinct-variant count, distinct-error-variant count (`error_variants`, the number of folded variants that classified as an error severity; omitted when zero), and first/last raw samples. Because semantically distinct failures can templatize identically (for example per-test `FAILED tests/test_<n>.py - assert <n> == <n>` lines), folding them into one family with only first/last samples would hide the other failures; `error_variants` preserves that hidden distinct-error count past sampling, error-bearing families rank ahead of benign families so the inline cap never truncates a failure family in favor of progress noise, and `digest_signal_text` names the count so it survives into the token-savings projection. Only families with more than one distinct variant (genuine near-duplicates that exact-line collapsing misses, such as cargo/build/install progress, dependency downloads, and repeated harness lifecycle lines that vary by one path, count, timestamp, or crate name) are reported, ranked error-families-first then by occurrence count and bounded to the top families with the full family count preserved. Likewise, classified error signals are ordered ahead of warnings before the inline signal cap applies, so a high-occurrence warning cannot crowd a distinct error out of the digest. `line_families` is omitted from JSON when empty.
4. Extract file anchors and symbol-like tokens from the transcript for quick follow-up lookups. Agent-doc runtime-style `file=...` and `path=...` fields count as file anchors even when they do not carry line numbers, but project-root/directory paths that normalize to an empty display path are ignored; timestamped event names plus `event=...`, `pane=...`, and `session=...` fields are retained as structured symbol refs.
5. When `.tsift/summaries.db` already has current rows for anchored files or extracted symbols, include up to two cached summary snippets; otherwise report `missing`, `stale`, or `unavailable` without mutating the cache.
6. When the raw log is bulky — at or above `LOG_DIGEST_RAW_ARTIFACT_MIN_BYTES` (4096 bytes), or when any signal/repeated-line/line-family/stack group overflows its inline cap so folded detail is hidden — persist the full transcript behind an artifact handle instead of losing it (stdin) or relying only on inlined groups. The standalone `log-digest` command writes the raw bytes to `.tsift/artifacts/<handle>.log` and attaches a `raw_log_artifact` ref carrying the handle, relative path, byte/line counts, and a `tsift log-digest --input <artifact> --json` expansion command; small logs attach nothing. `context-pack` surfaces the same `raw_log_artifact` shape on its bounded log preview, referencing the existing on-disk `--log-input` file rather than re-persisting it. This keeps the digest bounded by default while leaving the full raw transcript expandable through a stable handle. `raw_log_artifact` is omitted from JSON when absent.

`log-digest` is intentionally transcript-only.

### Log Digest Fixture Gate

`tsift log-digest --fixture <file> [--fail-under]` runs a fixture-backed token-savings and false-negative gate for the log digest across `cargo`, `pytest`, `npm`, `pnpm`, and `agent-doc` runtime logs.

```bash
tsift log-digest --fixture fixtures/log-digest-token-savings.json --fail-under --json
```

Behavior:

1. Each fixture case carries `name`, `ecosystem`, raw `input_lines`, a `minimum_savings_percent` token-savings floor, and `required_signals` / `forbidden_signals` substring lists.
2. The gate runs each case through the same `log-digest` engine, projects the bounded digest an agent reads (signal messages, line-family templates/samples, repeated lines, anchored file paths, symbol refs, warnings) via `digest_signal_text`, and estimates tokens deterministically at ~4 chars/token.
3. The **token-savings gate** requires `1 - digest_tokens/raw_tokens` to meet each case's `minimum_savings_percent`, proving the digest compresses bulky near-duplicate build/test/install noise rather than only claiming to.
4. The **false-negative gate** requires every `required_signal` substring to survive into the digest projection, proving real failures (`error[E0277]`, pytest `E   assert` / `FAILED ...`, `npm ERR!`, `ERR_PNPM_...`, `agent-doc exit: claude_exit code=1`) are never folded away as noise. The **false-positive gate** requires every `forbidden_signal` substring to be absent from the *classified signal messages* (`signal_message_text`), proving benign lines are not misclassified as errors. The forbidden check looks at classification output only, not the full projection, because a benign line's tokens can still surface as a file ref or symbol (for example a `err_pnpmish-fixture` symbol or an anchored path) even when the line is correctly not a signal. The generic signal classifier recognizes the cargo/pytest/npm/pnpm error forms in addition to the existing `error:` / `warning:` / panic / traceback shapes, with npm/pnpm `ERR!` / `ERR_PNPM_` detection anchored to whitespace tokens so substrings like `stderr!` do not false-positive. It also recognizes the canonical terminal-failure summary lines that carry no `error:` prefix — the cargo libtest `test result: FAILED` summary, test-runner counts (`<non-zero N> failed` as in pytest `1 failed, 3 passed` or jest `Tests: 2 failed`), GNU make `*** ... Error` lines, `Segmentation fault` / `core dumped`, and Rust assertion panics (`assertion failed:` and the `assertion \`left == right\` failed` form) — each token-anchored so passing summaries (`test result: ok ... 0 failed`, `0 failed`) and prose (`the build failed to ...`) are not misclassified.
5. `--fail-under` exits non-zero when any case misses its savings floor, drops a required signal, surfaces a forbidden one, or drops more distinct error-severity signal groups past the inline cap than `maximum_dropped_error_signals` allows (default 0). The dropped-error fidelity guard exists because the savings floor alone cannot catch lost signal: a capped digest always reports high savings against an unbounded raw log no matter how many distinct failures were truncated, so the gate also compares `error_signal_groups` against the inline error signals the digest actually surfaces and fails closed when distinct failures vanish without being acknowledged (the raw-log artifact still carries them). This is the CI gate that keeps log-digest compression honest and regression-safe. It does not execute the underlying command, and it keeps summary enrichment read-only so digesting verbose output never contends with `tsift summarize --extract`.

## Session Digest

`tsift session-digest` turns long session transcripts and harness runtime logs into bounded execution evidence for agent context.

```bash
tsift session-digest --path . < tasks/software/tsift.md
tsift session-digest --source claude-jsonl --input ~/.claude/projects/foo/session.jsonl --json
tsift session-digest --source codex-jsonl --input ~/.codex/sessions/2026/05/02/rollout-....jsonl --json
tsift session-digest --source agent-doc-log --input .agent-doc/logs/tsift-v0.1.log --json
```

Accepted sources:

- markdown session documents such as `agent-doc` / Codex task files
- Claude JSONL transcripts with `message.content` text/tool blocks
- Codex JSONL transcripts with `response_item` / `event_msg` records
- `agent-doc` runtime `.log` files with session start/restart/timeout/exit events

Behavior:

1. Read captured session input from stdin by default, or from `--input <file>`.
2. Auto-detect markdown, Claude JSONL, Codex JSONL, or `agent-doc` runtime logs unless `--source markdown|claude-jsonl|codex-jsonl|agent-doc-log` forces one parser.
3. Extract bounded prompt targets, shell commands, touched file paths, symbol-like identifiers, failure lines, runtime-event churn, and closeout evidence such as verification/install/commit/push/version mentions. File references are conservative: shell redirection fragments such as `2>/dev/null`, existing directories, and slash-separated conversational labels without a real file, known filename, or supported file extension are not reported as touched files. Runtime log path fields that point at the session root or another existing directory are not reported as touched files, so project-root `cwd_resolved` events cannot produce empty file anchors.
4. Ignore copied harness-instruction ballast such as markdown headings, placeholder slash-command examples, and bold imperative labels so prompt/failure hotspots stay focused on actual session work.
5. Treat successful test summaries, prompt directives, source-code snippets, and bare section labels as non-failures: lines such as `failures:`, `No failures detected`, `test result: ok. ... 0 failed`, `4 passed, 0 failed`, `do [#id] ... failure extraction ...`, and source lines like `panic!(...)` must not appear in session-digest failures or session-review unresolved failures, while real panic/assertion/error/exit evidence is preserved with its command/session anchors. Exit failures from command transcripts must name the parsed command, for example `cargo test exited with code 1`, instead of a generic `command exited with code 1`.
6. Keep the digest transcript-only: it summarizes what happened in the session, but it does not replay tool calls or attempt to reconstruct the full conversation.

`session-digest` is intentionally conservative. It favors bounded evidence over perfect transcript reconstruction so long agent sessions can be collapsed into compact handoff or review context.

## Session Cost

`tsift session-cost` turns Claude/Codex transcript usage and `agent-doc` runtime logs into bounded cost summaries for agent context.

```bash
tsift session-cost --input ~/.claude/projects/foo/session.jsonl --json
tsift session-cost --source codex-jsonl --input ~/.codex/sessions/2026/05/02/rollout-....jsonl
tsift session-cost --source agent-doc-log --input .agent-doc/logs/tsift-v0.1.log
tsift session-cost --fixture fixtures/real-session-prompt-cache-effectiveness.json --fail-under --json
```

Accepted sources:

- Claude JSONL transcripts with assistant `message.usage` payloads
- Codex JSONL transcripts with `event_msg` `token_count` records
- `agent-doc` runtime `.log` files with start/restart/timeout events

Behavior:

1. Read captured transcript/log input from stdin by default, or from `--input <file>`.
2. Auto-detect Claude JSONL, Codex JSONL, or `agent-doc` runtime logs unless `--source claude-jsonl|codex-jsonl|agent-doc-log` forces one parser.
3. Normalize prompt-side totals, cached-input totals, output totals, and largest per-turn outliers so token-heavy sessions can be compared without ad hoc `jq` pipelines. `session-cost` reports one transcript/log at a time; `session-review` keeps its bounded multi-session aggregate separate from the latest matched session's own cost summary so cached resend totals across many sessions are not mistaken for a single-session bill. Codex `token_count` records prefer `info.last_token_usage` when present, because newer rollouts can interleave more than one cumulative `total_token_usage` stream in a single JSONL file; duplicate cumulative snapshots are skipped, and older records without `last_token_usage` fall back to cumulative deltas.
4. Emit a bounded `prompt_cache_plan` when provider cache usage is observed or when uncached prompt volume is large enough to justify cache setup. The plan is diagnostic rather than a live model adapter: it records cache status, observed cached-read/creation tokens, stable-prefix invariants, provider adapter requirements, and next actions. It also includes bounded `analytics` for reviewing effectiveness over time: sample count, effective/stable/improving/declining trend, average/first/last cached-input ratios, net cached-read versus creation tokens, read/create ratio when creation tokens exist, likely invalidation diagnostics for cached-ratio drops, cache-creation spikes, and read/create regressions, a prompt-cache prefix drift report, and a first-plus-latest timeline of per-turn cache ratios. A compact `scorecard` groups the same economics by provider with sample count, net cached-read tokens, read/create ratio, trend, suspected invalidation cause, and the next `session-cost` command to rerun before a long agent-doc cycle. Each timeline call carries prompt-cache attribution metadata when usage is parsed: provider, optional cache key, stable-prefix fingerprint, explicit breakpoint paths, and optional routing affinity. Breakpoint paths are position-independent — JSON array indices are stripped (`message.content[N].cache_control` → `message.content.cache_control`) and repeated identical shapes collapse to a single counted entry (`... (xN)`) — so inserting a non-cached block ahead of the cached one does not shift the breakpoint identity and read as false cache-boundary drift. The prefix drift report compares adjacent calls by stable-prefix fingerprint, cache key, explicit breakpoint paths, routing affinity, and provider, then marks cache-ratio drops or creation spikes with the first changed field. Because the stable-prefix fingerprint is typically a *derived* composite of provider, cache key, stable prefix, and breakpoints, attribution orders the concrete sub-fields (cache key, breakpoints, routing affinity, provider) ahead of the composite fingerprint, so `first_changed_field` and the scorecard's suspected invalidation cause name the specific changed input; the fingerprint is reported as the first changed field only when no tracked sub-field moved — i.e. the stable prefix content itself drifted. The provider notes must distinguish Anthropic explicit breakpoints (`cache_control` on stable system/tool/user boundaries), OpenAI cache-key routing (`prompt_cache_key` for byte-identical thread prefixes), and self-hosted/edge affinity needs when cache locality is replica-bound. The invariants should preserve the core prompt-caching shape from agent runtimes: stable context first, append-only thread history until intentional compaction, no volatile values inside the cached prefix, and compaction calls that reuse the live warm prefix instead of starting a cold standalone summarizer.
5. In fixture mode, read a JSON fixture whose cases carry `source`, `input_lines`, `minimum_cached_input_ratio`, `minimum_net_cached_input_tokens`, and `maximum_read_create_regressions`. Fixtures can also declare top-level `required_regression_scenarios` plus per-case `regression_scenarios`, `required_prefix_drift_fields`, and `required_diagnostics`; the report emits required/covered/missing scenario lists and fails closed when required coverage disappears. Each case is parsed by the same `session-cost` engine as real transcripts. `--fail-under` exits non-zero when any case is missing prompt-cache analytics, falls below its cached-input ratio or net cached-token threshold, exceeds the allowed read/create regression count (counted from the raw cached/creation token totals, not the display-truncated diagnostics list, and the session-level regression diagnostic is reserved a slot so a long degraded session's per-turn diagnostics cannot truncate it away), lacks required provider-adapter evidence, omits required prefix-drift fields/diagnostics, or no longer covers the fixture's required regression scenarios. This is the CI gate for prompt-cache economics; it proves cache reads beat cache creation work instead of only reporting recommendations.
6. For `agent-doc` runtime logs, summarize bounded churn counters such as `fresh_restart`, `continue`, and `auto_trigger_timeout`, including the highest observed `restart_count`.
7. Derive bounded runtime-churn families from `agent-doc` logs so the digest can call out `fresh_restart`, `auto_trigger_timeout`, ctrl-d restart loops, and clean quit-after-eof exits without replaying the full raw event stream. Clean quit-after-eof exits are summarized for context but do not count as restart-loop warnings.
8. Summarize bounded loop clusters for repeated prompt bodies, repeated command bundles, and repeated closeout churn so common restart/retry patterns become explicit instead of hiding inside the top-N command/event lists.
9. Detect repeated source-file read tool calls in Claude/Codex transcripts, including native `Read` blocks and common shell reads such as `sed -n`, `cat`, `bat`, `head`, and `tail`. The report groups repeated reads by file path plus requested range, estimates total and duplicate token spend using a deterministic line-count heuristic, and emits concrete `tsift source-read ... --budget normal` plus `tsift summarize --file ...` follow-up commands so agents can switch to bounded source windows instead of re-reading the same file/range.
10. Emit guardrails when the session shows obvious budget risk: oversized prompt turns, very high cached-input resend ratios, restart-loop churn, or repeated `commit_already_current` no-op closeouts. `max_restart_count` is reported as context on real restart-churn guardrails, but it must not emit a restart-loop warning by itself when churn families such as `fresh_restart`, `auto_trigger_timeout`, or ctrl-d restart loops are absent. For newer `agent-doc` `document_cycle` logs, collapse repeated closeout lines to one occurrence per `(cycle, event)` before counting so retry noise does not swamp the summary. Each guardrail includes actionable compact/restart guidance.

`session-cost` is intentionally cost-focused. It does not reconstruct the full conversation or replay tool calls; it compresses token/runtime overhead into a bounded report you can paste into backlog triage, handoffs, or benchmark notes.

## Session Review

`tsift session-review` auto-discovers related Claude/Codex transcript logs plus `agent-doc` runtime logs for a document or repo path, then emits one bounded combined review.

```bash
tsift session-review tasks/software/tsift.md
tsift session-review --next-context tasks/software/tsift.md
tsift session-review src/tsift --json
```

Behavior:

1. Resolve the owning repo/submodule root for the target path.
2. For document targets, read `agent_doc_session` through the shared `tsift-agent-doc` session-markdown helper when present and use the matching `.agent-doc/logs/<session>.log` to learn historic `file=` aliases plus prior `session=` aliases before scanning other harness logs. Agent-doc markdown/log recognition plus backlog and queue row parsing are owned by `tsift-agent-doc` so `session-review`, `context-pack`, traversal, and rewrite consumers do not duplicate marker-level schema checks.
3. Discover related Claude sessions under `~/.claude/projects/<cwd-slug>/`, Codex sessions under `~/.codex/sessions/`, and `agent-doc` runtime logs under `<root>/.agent-doc/logs/`.
4. For directory targets, match candidate logs by cwd. For document targets, require a document-specific signal (`agent_doc_session` or a document path alias) before counting a Claude/Codex transcript; when cwd also matches, report it as supporting evidence instead of letting a shared workspace cwd count by itself. Candidate matching should use structured user/tool-input snippets rather than arbitrary transcript stdout so unrelated hook output or command dumps do not overmatch a shared workspace file name. Reuse the existing `session-digest` and `session-cost` parsers to aggregate prompt targets, commands, failures, closeout evidence, token totals, restart churn, and repeated loop clusters into one bounded report, including Codex `last_token_usage` accounting so interleaved cumulative streams do not inflate review-level token totals or largest-turn outliers.
5. Claude/Codex transcript parsing should skip malformed JSONL lines and ignore non-conversation attachment records where possible so one bad line or hook payload does not fail the whole review.
6. Session-review inherits session-digest's instruction-ballast, successful-test-summary, and failure-meta/progress filtering so copied harness docs, passing test output, assistant assessment prose, and CI/status commentary about false-positive failure groups do not dominate prompt/failure hotspots.
7. Session-review also carries forward aggregate session-cost guardrails so document-level reviews warn when token spend is mostly cached resend, restarts are looping, or closeouts are mostly no-ops. The review should also surface repeated prompt bodies, repeated command bundles, repeated closeout churn, and repeated source-file read diagnostics as explicit summaries instead of leaving that repetition buried inside broader aggregates. Repeated file-read diagnostics retain the path/range grouping, duplicate-token estimate, and concrete `tsift source-read` / `tsift summarize --file` follow-up commands from `session-cost`. When the source is an `agent-doc` runtime log, normalize `document_cycle` closeout details to `phase + event` and count them once per cycle so the review reports distinct closeout cycles instead of raw repeated retries.
8. Aggregate token, command, file, failure, guardrail, loop-cluster, closeout totals, and prompt-cache ROI rows over the same bounded newest matched session rows emitted in `sessions`; older matched transcripts are considered for discovery but do not inflate hidden review totals. The JSON report preserves the legacy top-level token fields for compatibility, adds `aggregate_cost` with `scope: "bounded_matched_sessions"`, adds `latest_session_cost` with `scope: "latest_matched_session"`, includes each session row's own `largest_turn_total_tokens`, and emits `prompt_cache_roi_scorecard` rows with session source/path, provider, net cached-read tokens, read/create ratio, trend, suspected invalidation cause, and the exact `tsift session-cost --source ... --input ... --json` command for the transcript. Human and compact output label aggregate token fields explicitly, print the latest-session total/largest-turn pair next to them, and include `prompt-cache-roi` rows when cache economics are present.
9. `--next-context` emits only the bounded resumable handoff pack: active prompt targets, the last verification closeout state, touched files/symbols, unresolved failures, session-level guardrail action rows, prioritized `next_token_actions`, and the next digest commands to run instead of replaying raw session/log history. Guardrail action rows use the `guardrail:<kind>` failure kind so restart-loop, prompt-budget, cached-resend, and no-op closeout warnings stay visible even when no command failure was extracted. When prompt-budget, cached-resend, restart-loop, or no-op closeout guardrails are present, `next_token_actions` maps them in priority order to exact compact, restart, digest, and `rewrite_commands`; agent-doc markdown targets include `agent-doc compact <file> --commit`, `agent-doc start <file>`, `tsift --envelope session-review <file> --next-context --budget normal`, and `tsift --envelope context-pack <file> --budget normal`. Repeated raw source/session/log reads add a `repeated_raw_read` action with concrete `tsift rewrite --run ...` and `tsift --envelope source-read ...` replacements; repeated command bundles add a `repeated_command_bundle` action with exact `tsift rewrite --run '<command>'` replacements for broad search/read/test/build commands. For agent-doc template documents with a live unresolved `agent:exchange` tail after the latest response boundary, prompt targets, touched files/symbols, and unresolved failures come from that tail rather than historical transcript aggregates; freeform live instructions that do not use a `do [#id]` or slash-command shape still count as active prompt targets and still suppress stale historical files/failures. Agent-doc markdown targets also carry an optional `agent_doc_queue` profile: the active queue head resolved to its backlog prompt, live unresolved exchange-tail lines, top unchecked backlog/review rows, compact frontmatter prompt preset refs, and expansion handles for refreshing next-context/context-pack or expanding the source document. Frontmatter prompt presets, examples, compacted/archive summaries, completed backlog entries, done-history rows, resolved `### Re:` responses, repeated resolved directives, stale/bogus paths from old matched sessions, instruction prose such as `After finalize...`, source snippets, assistant progress or assessment lines discussing failure extraction/classification false positives, and generic unknown-command exits must not reappear as current handoff failures. If no live document tail is available but an active queue head is present, it becomes the active prompt target; otherwise `session-review` falls back to the bounded aggregate review fields.

`session-review` is intentionally bounded. It does not replay full conversations; it gives one cross-harness review surface so document-level session analysis stops depending on ad hoc file hunting and manual aggregation.

### `context-pack`

`tsift context-pack <path>` turns the existing bounded session/diff/test/log surfaces into one resumable handoff payload for agent turns.

Example:

```bash
tsift context-pack tasks/software/tsift.md --test-input test.log --log-input build.log --json
```

Behavior:

1. Computes `session-review --next-context` for the target document or repo path.
2. Computes the current worktree `diff-digest` for the resolved repo root.
3. Optionally inlines `test-digest` when `--test-input <file>` is provided.
4. Optionally inlines `log-digest` when `--log-input <file>` is provided.
5. Emits the follow-up digest commands needed to refresh or expand the pack without replaying raw transcripts or verbose logs.
6. Includes current `status_reminders` from the resolved repo root, so a stale index or missing summary cache remains visible in context-pack JSON and human output without requiring a separate `tsift status` call.

`context-pack` is intentionally bounded by default: it emits preview-style lists plus counts rather than dumping the full underlying reports, and `--max-items` / `--max-bytes` further tighten the preview envelope for high-token-pressure turns. For agent-doc markdown targets, the composed `next_context.agent_doc_queue` profile preserves the active queue prompt and nearby live rows while omitting archive summaries, done history, and stale session transcript context by default. Its symbol-bearing preview lists keep the raw `touched_symbols` strings for compatibility while also adding compact symbol-ref objects with stable `handle` ids and canonical `tag_alias` values for `next_context`, diff previews, and log symbol references. If tagpath ontology docs exist under `.naming/tags/*.md`, `context-pack` also loads them once and attaches compact `ontology_refs` to matching symbol refs, summary refs, and the top-level pack; those refs carry handle/tag/path metadata so stable domain vocabulary can be referenced without inlining repeated prose definitions. When the underlying diff/test/log digest already found current cached summaries, the corresponding touched file, failure, signal, file-ref, and symbol/tag-alias family rows expose bounded `summary_refs` with stable handles plus `tsift summarize --file ...` or `tsift summarize <symbol>` expansion commands, so resumptions can keep summary context behind handles instead of inlining every cached summary body.
