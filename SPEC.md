# Spec: AST-Aware Code RAG

## Goal

Extend tsift with tree-sitter AST parsing, dependency graph tracking, and per-submodule isolation to enable token-efficient code retrieval at the function/symbol level rather than file level.

## Architecture

```
tsift (root crate — public package shim: lib.rs + graph/lang/resolution/substrate/libsql_backend
│        modules only `pub use` sibling crates; the package binary delegates to tsift-cli)
├── tsift-core crate (packages/tsift-core — provider-neutral graph types)
│   ├── GraphNode, GraphEdge, GraphProjection, GraphPath, GraphSubgraph
│   ├── GraphProvenance, GraphFreshness, GraphPropertyFilter
│   ├── GraphQueryOptions, GraphQueryPage, GraphPagedSubgraph
│   ├── NeighborhoodScoring, RankedNeighborhoodOptions, RankedNeighborhoodResult
│   ├── GraphStore trait (CRUD/query contract — lookup, kind scans, neighborhoods, ranked neighborhoods, shortest paths)
│   ├── ConvexGraphClient trait, ConvexRowsGraphClient, ConvexGraphStore
│   ├── ConvexProjectionRows, ConvexNodeRow, ConvexEdgeRow
│   ├── SQLITE_GRAPH_SCHEMA_VERSION (shared schema version constant)
│   └── shortest_path_using_outgoing, apply_graph_query_page helpers
├── tsift-sqlite crate (packages/tsift-sqlite — SQLite graph store backend)
│   ├── re-exports all tsift-core types for backward compatibility
│   ├── SqliteGraphStore (graph_nodes, graph_edges, graph_node_properties, projection versions, tombstones)
│   ├── SqliteProjectionRefresh, SqliteProjectionVersion, SqliteProjectionRefreshPhase
│   ├── open_graph_read_only_connection, open_graph_read_only_connection_resilient
│   ├── ReadOnlyRecovery, snapshot copy utilities (shared with index module)
│   └── projection boundary for FalkorDB/other read models
├── tsift-libsql crate (packages/tsift-libsql — libSQL graph store backend, optional)
│   ├── LibsqlGraphStore (local and remote libsql/Turso)
│   └── implements GraphStore trait from tsift-core
├── tsift-surrealdb crate (packages/tsift-surrealdb — SurrealDB graph store backend spike, optional/excluded from default workspace)
│   ├── SurrealdbGraphStore (embedded SurrealKV file-backed or in-memory)
│   ├── writes provider-neutral ConvexProjectionRows into SurrealDB records
│   └── implements GraphStore trait from tsift-core behind `backend-surrealdb`
├── tsift-graph crate (packages/tsift-graph — language-aware graph extraction)
│   ├── lang module — Lang enum, Symbol, tree-sitter symbol/call queries, extract_symbols
│   ├── graph extraction — call sites, routes, edge resolution, community detection, shortest path
│   ├── TerseCommunityMember, TerseCommunity, TerseCommunityResult — compact community serialization (name + tagpath_handle only, top-N member slice)
│   ├── complexity module — ComplexityMetrics, LanguageExtractor trait, LanguageRegistry
│   └── re-exported via src/graph.rs and src/lang/mod.rs as thin shims
├── tsift-algorithms crate (packages/tsift-algorithms — graph algorithms)
│   ├── graph_builder module — shared Graph struct, build_graph() (node index + directed adjacency), build_node_index() (node index only); reused by health, dead_code, scc
│   ├── scc module — iterative Tarjan SCC (strongly connected components)
│   ├── health module — composite health score (connectivity, reachability, centrality, cycle risk); terse_health_report computes top/bottom N scores directly without allocating full per-node sub-metrics
│   ├── dead_code module — dead code detection (unreachable, isolated, orphaned nodes)
│   ├── coupling module — coupling analysis (fan-in, fan-out, instability metrics per module)
│   ├── surfaced by `tsift analyze` over the indexed call graph
│   └── re-exported via root `tsift` as `tsift::algorithms`
├── tsift-tokensave crate (packages/tsift-tokensave — tokensave DB reader adapter)
│   ├── TokensaveDb — read-only adapter for .tokensave/tokensave.db
│   ├── schema mapping: tokensave nodes/edges → tsift GraphNode/GraphEdge
│   ├── FTS5 search via nodes_fts virtual table
│   ├── implements GraphStore trait (read-only, write ops bail)
│   ├── selectable through `tsift graph-db --backend tokensave`
│   └── re-exported via root `tsift` as `tsift::tokensave`
├── tsift-resolution crate (packages/tsift-resolution — multi-strategy resolution, scoring, blocklist)
│   ├── scoring module — RankedNeighbor, neighborhood ranking, edge kind scoring, heuristic helpers
│   ├── blocklist module — generated artifact detection, planner config path filtering
│   └── resolve module — F1 scoring, token-overlap ranking, NodeMatchKind, kind priority
├── tsift-quality crate (packages/tsift-quality — quality-gate surfaces)
│   ├── audit module — skill drift detection, manifest reconciliation, usage scanning, cleanup, report writing
│   ├── perf_gate module — perf-gate workload definitions, hop-cap tiers, baseline backend selection
│   ├── dci_benchmark module — DCI (Driven Causal Index) benchmark harness types and result rollups
│   ├── runtime_churn module — RestartChurnState / RestartChurnSummary derivation from transcript events
│   ├── lint module — markdown lint, project root resolution (depends on tsift-index for config + IndexDb)
│   └── re-exported via root `tsift` `pub use tsift_quality::{audit, dci_benchmark, lint, perf_gate, runtime_churn};`
├── tsift-index crate (packages/tsift-index — config + project walk + init + AST symbol index)
│   ├── config module — Config + workspace/submodule resolution
│   ├── walk module — file walking + mtime-based prune semantics + language tagging
│   ├── init module — instruction injection, OpenCode/Codex hook setup, npm package parity
│   ├── index module — AST symbol/index DB management, writer/reader, lock probes, snapshot fallback
│   └── re-exported via root `tsift` `pub use tsift_index::{config, index, init, walk};`
├── tsift-agent-doc crate (packages/tsift-agent-doc — agent-doc/session observability domain)
│   ├── session_cost module — token/runtime-cost digest for Claude JSONL, Codex JSONL, agent-doc logs
│   ├── session_digest module — bounded session transcript/log digest with restart-churn families
│   ├── session_review module — cross-harness aggregate review combining digest + cost + log discovery
│   └── re-exported via root `tsift` `pub use tsift_agent_doc::{session_cost, session_digest, session_review};`
├── tsift-memory crate (packages/tsift-memory — first-party memory substrate)
│   ├── owns `.tsift/memory.db` schema versioning for memory events, summaries, artifacts, tool spans, embeddings, graph links, and import runs
│   ├── exposes token-budgeted handoff planning so observer/plugin prompts split before model calls instead of overflowing
│   ├── exposes a budget guard that rejects oversized raw tool/log/transcript payloads, replaces them with digest/context/session-review commands, and emits retryable chunk plans
│   ├── defines agent-doc hook event contracts for prompt targets, tool artifacts, response summaries, closeout proof, and session-check results
│   ├── projects first-party `.tsift/memory.db` rows into provider-neutral graph nodes/edges (`memory_session`, `memory_event`) plus semantic/source rows for graph-db retrieval
│   ├── reads the observed `claude-mem` SQLite tables (`observations`, `session_summaries`, `user_prompts`) without mutating them and can optionally migrate all supported rows as imported memory events before graph projection, with per-table source/read/import reconciliation
│   ├── exposes a query packet contract for future ranked, token-capped memory retrieval
│   └── re-exported via root `tsift` as `tsift::memory`
├── tsift-session crate (packages/tsift-session — compatibility shim)
│   └── re-exports `tsift-agent-doc::{session_cost, session_digest, session_review}` for existing consumers
├── tsift-summarize crate (packages/tsift-summarize — cached LLM analysis foundation)
│   ├── summarize module — SummaryDb (read-only / read-write opens), entities/relationships/concepts JSON, snapshot fallback for rollback-journal contention, Anthropic API extract pipeline
│   ├── shared by tsift-digest (diff/log/test consume cached summaries) and the tsift-search crate
│   └── re-exported via root `tsift` `pub use tsift_summarize::summarize;`
├── tsift-digest crate (packages/tsift-digest — code-aware digest emitters)
│   ├── diff_digest module — worktree/staged/revision diff digest, touched symbols, call-edge deltas (uses tsift-graph)
│   ├── log_digest module — bounded verbose-log digest, repeat collapse, signal grouping
│   ├── metric_digest module — repeated metric-run deltas + news tables (self-contained)
│   ├── test_digest module — grouped test-failure digest (cargo/pytest)
│   ├── depends on tsift-graph (diff edges + Lang), tsift-quality (lint/runtime_churn), tsift-summarize (SummaryDb enrichment)
│   └── re-exported via root `tsift` `pub use tsift_digest::{diff_digest, log_digest, metric_digest, test_digest};`
├── tsift-search crate (packages/tsift-search — search ranking, impact analysis, tagpath annotation)
│   ├── sift module — local lexical search adapter (ranked BM25-ish lexical hits, cache serialization)
│   │   ├── TokenIndex — inverted token→files index for pre-filtering; skips files with no matching query tokens; cached as token-index.json in cache_dir
│   │   └── Sift::search builds/loads TokenIndex automatically; only reads+scores files in the token-match set
│   ├── impact module — change-impact analysis (call-edge/route/import impacts; per-language import detection gated by lang-* features)
│   ├── tagpath_adapter module — tagpath `.naming/index.json` family/member lookup + handle round-trip
│   ├── depends on tsift-index (config/index/walk), tsift-digest (diff_digest), tsift-graph (Lang), tsift-quality (lint), tsift-summarize
│   ├── forwards lang-* features to tsift-graph (mirrors root tsift) so impact's per-Lang arms compile
│   └── re-exported via root `tsift` `pub use tsift_search::{impact, sift, tagpath_adapter};`
├── tsift-status crate (packages/tsift-status — session health + lock diagnostics)
│   ├── status module — index freshness, instruction-version check, summary-cache recovery, lock-sidecar/journal state
│   ├── backs `tsift status` and `tsift locks`
│   ├── depends on tsift-index (config/index/init), tsift-sqlite (sidecar/recovery helpers), tsift-summarize (SummaryDb)
│   └── re-exported via root `tsift` `pub use tsift_status::status;`
├── tsift-cli crate (packages/tsift-cli — CLI dispatch, command handlers, output formatting)
│   ├── depends directly on sibling `tsift-*` crates; must not depend on the root `tsift` re-export shim
│   ├── clap CLI types — Cli, Commands, GraphDbQuery, output format enums
│   ├── command handlers — cmd_search, cmd_index, cmd_graph, cmd_communities, cmd_analyze, cmd_explain, etc.
│   ├── output formatting — ToolEnvelope, ResponseBudget, terse/schema transforms
│   ├── tagpath annotation — annotate_hits/stored_symbols/edges/communities/path_nodes_with_tagpath
│   ├── traversal graph — TraversalGraphBuild, exploration budget, worker packets
│   ├── convex sync — chunk planning, transport, snapshot diffing
│   └── binary entry point — src/main.rs delegates to tsift_cli::run()

└── rusqlite (storage — existing)
```

## Provider-Neutral Graph Substrate

The graph substrate is a content-agnostic layer below tsift. It stores typed property graph records:

- nodes: stable `id`, `kind`, `label`, string properties, provenance, and freshness
- edges: `from_id`, `to_id`, `kind`, string properties, provenance, and freshness
- provenance: source system plus source reference, with optional content hash
- freshness: optional content hash and observation timestamp for rebuildable projections

The stable API surface is the `GraphStore` contract plus the `tsift graph-db` CLI. The contract supports node upsert/delete, edge upsert/delete, node lookup, kind scans, stable edge-id lookup, outgoing and incident edge scans, edge property scans, scoped edge scans, bounded directed neighborhoods, and shortest paths. `tsift graph-db` exposes those reads over the local SQLite graph store, a freshness-checked Convex snapshot, or the read-only tokensave adapter at `.tokensave/tokensave.db`; `graph-db --json schema` returns the stable node/edge/operation JSON shapes. `tsift analyze` runs the algorithms crate over indexed call edges and reports SCCs, composite graph health, dead-code reachability from explicit or inferred entry points, and module coupling. Local SQLite graph writes use WAL mode, a bounded busy timeout, and a small WAL autocheckpoint budget; projection refreshes stage the next projection into temp tables, derive materialized `graph_node_properties` and `graph_edge_properties` rows from staged JSON inside SQLite for indexed property filters, persist stable edge ids, per-row hashes, and last-change source watermarks, prefilter unchanged node/edge/property rows before `ON CONFLICT` work, delete removed rows through anti-joins, cache refresh-produced operator counts/file/freelist proof, and keep transactional old-or-new visibility for readers. Refreshes prune tombstones for rows that reappear while retaining deletion tombstones for absent rows until snapshot-based Convex reconciliation can prove the remote consumer no longer needs them. Operator status/doctor reports reuse cached refresh counts for live row/tombstone totals, graph.db file-size/freelist impact, compaction proof, and non-fatal retention diagnostics when tombstones outnumber live graph rows instead of rescanning the full graph for ordinary status. Read-only graph-db status/query/evidence/doctor paths open the live database read-only and, when SQLite reports a rollback-journal or WAL-sidecar lock, retry against a temporary snapshot copy that includes `-wal` / `-shm` sidecars. Snapshot recovery is surfaced as a `recovery` JSON field or warning/doctor diagnostic so operators know a live writer is wedged without losing read-only graph evidence. `graph-db --json refresh` explicitly materializes `.tsift/graph.db` for operator workflows without requiring a throwaway `traverse` call, then reports projection version, content hash, source watermark, refresh mode (`cold_source_graph_rebuild` or `cached_source_watermark_reuse`), row counts, tombstone counts, delta upsert/delete/unchanged counts for nodes, edges, and materialized property rows, refresh phase timings including node and edge property-row staging, pruned tombstones, compaction policy, and next commands for `doctor`, `drift`, and `convex-sync`; when the summary cache is empty it emits an actionable warning that semantic rows are unavailable until `tsift summarize --extract <scope>` is run and the graph is refreshed again. `graph-db --json status` reports the same projection metadata, counts, and compaction policy without refreshing. `graph-db --json compact` returns the safe post-reconciliation compaction plan; `graph-db compact --apply` runs a WAL checkpoint and `VACUUM`, while tombstone deletion requires `--prune-tombstones --confirmed-convex-reconciled` after the remote Convex consumer has reconciled deletion rows. `graph-db --json evidence <backlog-id-or-job-handle>` returns a bounded worker handoff packet with the target node, reachable `worker_context` rows, reachable `source_handle` rows, reachable `worker_result` rows, reachable semantic rows, shortest paths, next commands, replay commands, repair commands, and compiled fixture coverage. `graph-db --json related <phrase>` is the general-knowledge retrieval surface: it compares natural-language phrases against cached `semantic_concept` / `semantic_entity` embeddings, uses the best matches as seed nodes, then expands a score-ranked and capped set of incident plus outgoing GraphStore edges around those seeds so semantic/source/caller/callee links survive high-degree neighborhoods before low-signal edges. This reuses neighborhoods as the graph expansion phase without replacing stable node-id neighborhood pagination or requiring camelCase/snake_case/tagpath query terms. Evidence target resolution is scoped to graph evidence node families (`backlog`, `job_packet`, `worker_result`, `worker_context`, `source_handle`) unless the caller supplies an exact graph node id; SQLite resolves non-id targets through indexed `graph_node_properties` lookups for `handle`/`ref_id` plus kind+label ordering instead of Rust-side family scans. Reachable context collection uses batched GraphStore traversal where the SQLite store executes one recursive CTE for all requested evidence row families instead of per-family or per-node outgoing-edge and node lookups. Conflict-matrix scoped snapshots reuse the evidence graph and fetch all edges whose endpoints are already in scope through `GraphStore::edges_between_nodes`, so SQLite performs bounded `IN`-chunk scans instead of one outgoing-edge query per scoped node. Read-only graph-db queries reuse a current materialized `.tsift/graph.db`; if evidence cannot resolve the requested target, it refreshes once, and when the CLI `--path` points to an agent-doc markdown file the refresh projects that session file instead of walking every markdown document in the repository. Tokensave-backed graph-db queries do not refresh or write projections; unsupported evidence packets fail closed with guidance to use node/kind/edge/neighborhood/path reads. Backlog and queued-job targets that have no code-token matches still project a session-line `worker_context` and `source_handle`, so evidence distinguishes "known backlog with only document context" from a broken projection. If a queue line still advertises an active `do #id` packet after a completed worker result for the same backlog id is already projected, evidence emits queue-head drift repair guidance that marks/reaps the item done through agent-doc closeout and explicitly avoids redispatching or reactivating the completed queue item. `graph-db --json doctor` validates the existing local `graph.db`, supplied Convex snapshot, or tokensave database without refreshing first, reports stale projection metadata, SQLite schema drift, tombstone retention warnings, compaction policy checks, orphan edges, duplicate ids, missing Convex index metadata, read-only snapshot recovery diagnostics, tokensave open/count diagnostics, and concise repair commands, then exits non-zero when any check must fail closed. `graph-db --backend convex-snapshot --convex-snapshot <rows.json> --json drift` refreshes the local SQLite projection, compares it with the supplied Convex snapshot, summarizes node/edge upserts, edge-before-node tombstones, stale projection metadata, orphan/duplicate failures, required-index gaps, and operator next commands before `convex-sync --apply` or Convex-backed graph reads. Kind scans and neighborhoods support deterministic node-id cursor pagination plus repeatable `--property KEY=VALUE` filters; edge and incident scans support deterministic edge-id cursor pagination plus repeatable edge `--property KEY=VALUE` filters; SQLite pushes kind scans, recursive neighborhoods, cursor limits, and materialized node/edge property filters into SQL and returns query-plan diagnostics proving the property indexes are used. Incident edge scans use UNIONed `from_id` and `to_id` probes so SQLite can plan `idx_graph_edges_from_kind` and `idx_graph_edges_to_kind` independently instead of depending on an OR predicate. Path reads accept `--max-hops N` for bounded directed path searches; SQLite uses a batched frontier BFS over `idx_graph_edges_from_kind` rather than recursive string-path concatenation, preserving caps for deep-chain/high-degree fixtures and backend-eval `path_max_hops` metric gates. The compiled CLI conformance suite exercises the same graph-db refresh/status/evidence/schema/edge/edges/incident/kind/neighborhood/related/bounded path/drift/doctor/compact queries against SQLite and generated Convex snapshots, and exercises tokensave node reads through the same provider-neutral report surface; it locks result ordering, pagination/filter parity, fail-closed freshness checks, newer-schema rejection, rollback after failed refreshes, edge-before-node tombstone reconciliation, missing-node/duplicate-id rejection, diagnostic repair commands, large synthetic graph caps, SQLite query-plan index usage for edge lookup, edge property scans, and UNION incident scans, batched scoped edge collection, bounded graph-evidence traversal on large session backlog fixtures, session-hinted refreshes, queue-head drift guidance for completed worker results, graph-db lock snapshot recovery, and graph-orchestration contract parity against live CLI reports.

SQLite neighborhood paging builds the reachable node page and page-scoped edges from one recursive reachable-set CTE so node paging and edge materialization do not repeat the bounded walk. The JSON report also includes additive `ranked_neighbors` scored by traversal depth, edge kind, community co-membership, semantic relation evidence, fresh source handles, duplicate-name precision, and page handle coverage, then capped to a small preview so high-signal neighbors are available without turning ranked output into a second unbounded traversal. The primary `nodes` output remains ordered by stable node id for deterministic cursor pagination; changing that default is held behind the community-search quality gate, including real and synthetic_multi_module workloads plus handle coverage, duplicate-name precision, stale/no-tagpath behavior, top-community stability, and latency regression metrics. The conformance suite locks this shared-CTE diagnostic alongside SQLite and Convex snapshot parity.

**Ranked neighborhood** (`GraphStore::ranked_neighborhood`) scores neighbors during traversal and prunes low-scoring nodes before expanding them, bounding the result to `max_nodes` without first collecting the full unranked subgraph. Three configurable scoring strategies control priority ordering:

| Strategy | Scoring | Use case |
|----------|---------|----------|
| `BreadthFirst` | Depth-based decay (120 − depth×18) | Default; matches original BFS ordering, prunes deep nodes early |
| `EdgeKindWeighted` | Depth decay + edge kind weight (same weights as `ranked_neighbors`) | Prioritizes semantic/call edges over generic edges |
| `DegreeWeighted` | Depth decay + low-degree bonus (≤3 edges: +20, ≤10: +10) | Prefers leaf-like nodes in sparse neighborhoods over hubs |

The `RankedNeighborhoodOptions` struct carries `depth`, `max_nodes`, `scoring`, and an optional `edge_kind` filter. The result includes `pruned_count` and `total_discovered` diagnostics so operators can observe how many nodes were dropped by the budget. This achieves an estimated 30–60% token reduction for `explain` and `neighborhood` previews by avoiding the full unranked traversal + post-hoc scoring path.

Property-filtered edge scans must drive from `graph_edge_properties(key, value, edge_key)` into `graph_edges(edge_key)` instead of scanning the full edge table first. Backend-eval must choose its edge-property probe through the same indexed property surface rather than preloading all edges. Page diagnostics report the primary materialized-property cardinality before edge-kind/cursor paging so backend-eval and operator traces can prove the selective scan shape.

GraphStore exposes cheap count and sample-edge probes so status and backend-eval path selection can use SQLite `COUNT(*)` / indexed `LIMIT 1` queries instead of loading all graph rows before the measured operation.

The first implementation is a local SQLite store with `graph_nodes`, `graph_edges`, `graph_node_properties`, `graph_edge_properties`, `graph_projection_versions`, and `graph_tombstones` tables. SQLite is the development/offline/test correctness engine and a portable projection target; it is not the only intended backend. Refreshes are transactional: readers see the old projection or the new projection, never a half-swapped graph. Projection version rows record the traversal projection version, content hash, and source watermark; graph node/edge rows persist stable ids, row hashes, and the source watermark from the refresh that last changed that row, allowing subsequent refreshes to skip unchanged rewrites. Refresh staging loads graph node and edge rows into temp tables through bounded multi-row chunks before the SQL-side delta comparison. Materialized node-property and edge-property rows are maintained transactionally and refreshed by key-level delta upsert/delete operations, so hot property filters do not scan JSON payloads; refresh staging expands property JSON only for new or changed owner rows and reuses materialized property rows for unchanged row-hash owners. Row-level tombstones record removed nodes and edges so incremental projection consumers can reconcile deletions, while refresh-time tombstone compaction only removes tombstones whose row key is live again. Opening a `graph.db` fails closed if the file advertises a newer schema version than the current binary supports.

`tsift graph-db --json backend-eval` is the promotion gate for experimental substrate backends. It currently evaluates DuckDB/DuckPGQ, FalkorDB, Ladybug, SurrealDB, and Vela-Engineering/kuzu read-only prototypes by loading the same provider-neutral rows behind the `GraphStore` contract, then benchmarks SQLite and each candidate on the bounded path-hinted real projection plus synthetic high-degree and deep-chain graphs; `--full-projection` adds an opt-in full-project dataset so small session projections cannot hide large-graph regressions. Path-hinted task documents named after a workspace scope, such as `tasks/software/tsift.md`, use that scope's code index for the bounded real projection and source watermark; session projections materialize files, symbols, routes, session nodes, queue/job/result context, and token mention edges, but skip the global indexed call-edge graph. `--full-projection` remains the complete-call-graph guard for changes that need whole-project topology evidence. The benchmark covers refresh-equivalent projection load, status/count reads, stable edge lookups, edge property scans, incident scans, semantic-seeded `related` retrieval, bounded `path_max_hops` probes configured for the 64-hop default plus measured 128/256/512-hop tiers, adaptive one-hop probes on high-degree/direct-edge cases, evidence target resolution, `graph-db evidence`, `conflict-matrix`, and `dispatch-trace`; candidate output is compared against SQLite signatures for parity. The compiled conformance suite locks the SurrealDB prototype to SQLite-equivalent status/count rows, edge lookup, edge-property scan, incident scan, neighborhood, related, path-tier, evidence, conflict-matrix, and dispatch-trace results and metrics on every backend-eval dataset. Higher hop tiers are benchmark evidence only: backend-eval keeps user-facing defaults at 64 hops and requires real plus full-projection plus synthetic deep-chain 128/256/512-hop regression and row-usefulness metrics before any higher default can be considered. Candidate row adapters maintain node-kind and outgoing-edge indexes and use batched reachable-kind traversal so the prototype read path is not penalized by full edge scans. Reports include refresh phase timings for source graph build, projection row construction, SQLite open, SQLite temp staging, SQLite property-row staging, SQLite delta/tombstone/property writes, stats-cache update, and shared conflict-matrix preparation split into cache lookup, session-review compute (decomposed into `target_context_build`, `session_discovery`, `session_digest_total`, `session_cost_total`, `session_aggregation`, and `report_assembly` sub-phases reported as `conflict_matrix_preparation.session_review_compute.<sub>` so future hotspot reduction work can prove which inner step costs the dominant preparation seconds; `session_discovery` stat-walks Claude/Codex JSONL directories, keeps only the `MAX_RECENT_CANDIDATES_PER_SOURCE=64` most-recent files per source, and header-gates each per-file read by extracting the harness-specific `cwd` from the first 256 KB so files whose cwd does not match the target are skipped before any full read), status/index gate (decomposed into `prepare_agent_doc_index_gate`, `context_pack_status_reminders`, and `load_tag_ontology_preview_context` sub-phases reported as `conflict_matrix_preparation.status_index_gate.<sub>`; `prepare_agent_doc_index_gate` is wrapped in an in-process cache keyed by `(root, path_hint, scope, packet_label)` so multi-call pipelines reuse one inspection per process), context-pack diff, exploration materialization, graph orchestration, staged diff, and impact phases (decomposed into `context_resolution`, `diff_digest`, `test_path_scan`, `index_open`, `call_edge_impacts`, `route_handler_impacts`, `import_impacts`, and `report_assembly` sub-phases reported as `conflict_matrix_preparation.impact.<sub>`; the three iteration phases short-circuit when `changed_symbols` or `changed_tokens` is empty so the typical no-staged-changes cold path does not load every call edge or walk every source file); per-operation timings; adaptive path-budget configuration; metric-digest-ready numeric `metrics`; row-count-normalized `duration_micros_per_1k_graph_rows` metrics for every backend operation and real refresh phase; a `metric_digest_command`; and a `performance_gate` pointing at `fixtures/graph-db-performance-history.json`. The performance gate requires at least three repeated backend-eval samples before treating small source-graph-build, evidence-target-resolution, evidence-latency, path-tier, related-retrieval, full-projection, or candidate-backend deltas as meaningful, and it emits a `repeated_sample_command` that streams those samples through `metric-digest`. Reports also include projection-load notes, read-only lock/concurrent-writer behavior notes, install-portability notes, and a promotion decision with an explicit gate checklist. Backend-eval prepares evidence packets, graph node/edge snapshots, worker-result/source-handle expansion, semantic rows, and related retrieval seeds once per dataset/backend and reuses them for the conflict-matrix and dispatch-trace metric operations instead of rewalking the same orchestration graph for each measured surface. Backend-eval records a source watermark over the code-index snapshot, markdown session metadata, and summary cache metadata; when the watermark matches the current `.tsift/graph.db` projection, repeated runs reuse the cached provider-neutral projection and report skipped source-graph/projection/SQLite-write phases instead of rescanning unchanged source rows. The opt-in full-project projection dataset is cached under `.tsift/backend-eval-cache` as compressed JSON by the same generated-artifact-free source watermark, excluding `.tsift`, `.agent-doc`, and build-output paths; cache hits prune stale pre-fix JSON artifacts and reports include `full_projection.cache_lookup`, `full_projection.source_graph_build`, `full_projection.projection_rows`, `full_projection.cache.disk_bytes`, and `full_projection.cache.compression_ratio` metrics so repeated `--full-projection` samples can prove cold versus cached costs and disk footprint separately. On a full-projection cache hit, `full_projection.source_graph_build` and `full_projection.projection_rows` are required to report `0us`, proving unchanged symbol and summary inputs reused the cached provider-neutral rows instead of rebuilding the source graph. Conflict-matrix preparation also exposes a source/document/staged-diff cache key and `.tsift/conflict-matrix-cache` status so repeated CLI invocations can reuse the same context-pack, staged diff, impact, evidence, and target-scoped graph packet when those watermarks and graph freshness match; cache hits report the reused session-review, status/index, context-pack diff, staged-diff, and impact phases as 0us skipped work with the watermark guard in each phase detail instead of replaying stale compute durations. DuckDB/DuckPGQ, FalkorDB, Ladybug, Kuzu, and SurrealDB prototypes intentionally remain dependency-free in the default Rust build/install path while recording the future production requirement that native adapters stay optional and prove projection writes/load plus concurrent writer semantics before any SQLite replacement. A backend can only become eligible when it preserves SQLite parity and beats SQLite for every measured operation on every dataset without weakening SQLite's bundled install story or multi-process lock behavior; prototype-only adapters remain held until a real engine adapter satisfies that gate. `performance_gate.backend_adapter_spike` records the FalkorDB/Kuzu/SurrealDB adapter-spike contract directly in the report: a real optional adapter must prove provider-neutral projection writes/load, full `GraphStore` parity, writer/read-only lock behavior, default build/install portability, and faster-than-SQLite results across the real, full-projection, high-degree, and deep-chain workloads before read-only prototype evidence can become promotion evidence.

Backend-eval includes a bounded `neighborhood` operation for the real, synthetic high-degree, synthetic deep-chain, and opt-in full-projection workloads. The performance gate treats repeated neighborhood latency samples like evidence/path samples: single runs are diagnostic, while three-sample real/full/synthetic evidence is required before promoting a neighborhood traversal rewrite or backend replacement.

### Graph DB Performance Release Gate

The Graph DB performance release gate turns repeated `graph-db backend-eval` samples into a binding promote/block decision for candidate `GraphStore` backends. The gate is implemented in `src/perf_gate.rs` and exercised by `tests/perf_gate.rs`; the canonical sample store is `fixtures/graph-db-performance-history.json` and the canonical digest path is `tsift metric-digest --baseline fixtures/graph-db-performance-history.json`.

**Required workloads.** Every gate evaluation runs against four workloads, identified by their fixture metric prefix:

| Fixture metric prefix     | Gate workload name | Purpose                                                                 |
|---------------------------|--------------------|-------------------------------------------------------------------------|
| `real`                    | `default`          | Path-hinted real projection — the day-to-day operator workload          |
| `full_projection`         | `full-projection`  | Opt-in whole-project projection guard against large-graph regressions    |
| `synthetic_high_degree`   | `high-degree`      | Synthetic fan-out stress for adaptive one-hop / direct-edge cases       |
| `synthetic_deep_chain`    | `deep-chain`       | Synthetic deep-chain stress for bounded `path_max_hops` tiers           |

A workload that is absent from history is treated as `missing` and blocks promotion. A workload with fewer than three samples is treated as `insufficient_samples` and also blocks promotion. The minimum sample count is `perf_gate::MIN_SAMPLES_PER_WORKLOAD = 3`.

**Three-sample requirement.** No regression or improvement call is binding until each gate workload has at least three recorded samples from a single backend-eval revision range. The gate uses per-metric medians across the available samples, not single-sample arithmetic means, so a single outlier run cannot flip the decision. `backend-eval` already emits a `repeated_sample_command` that streams successive runs through `metric-digest`; the gate consumes the same history file rather than re-running samples itself.

**Full-projection cache-hit gate.** Full-projection samples are binding promotion evidence only after a cold populate leg is followed by cache-leg samples with `full_projection.cache.hit=1`. Cache-miss samples remain diagnostic for source-watermark drift and projection/write costs, but they cannot justify hop-cap changes or backend promotion.

**Promotion rule.** A candidate backend (FalkorDB, Kuzu, SurrealDB, DuckDB/DuckPGQ, Ladybug, ...) is promoted only when it beats the stabilized SQLite baseline on **every** gate workload across **every** required metric and at least matches SQLite on **every** observed lock-behavior metric. The required metrics today are `refresh.duration_micros` (projection write cost — proves the candidate is not trading slow writes for faster reads) and `total_duration_micros` (aggregate workload cost). Lock-behavior is enforced through any metric whose suffix is `lock_wait_micros` or `lock_contention_micros`; when the candidate reports such a metric for a workload, its median must be ≤ SQLite's median for the same metric. Any single workload regression, missing baseline sample, or absent gate workload returns `block`. The candidate stays blocked until every workload returns `beats` for the entire required metric set.

**Hop-cap promotion rule.** Raising the user-facing graph path default above 64 hops is a separate gate from backend promotion. The current default remains `64` until `perf_gate::evaluate_hop_cap_promotion` returns `promote` for the candidate tier. The gate evaluates the SQLite baseline only and requires at least three samples on `real`, `full_projection`, and `synthetic_deep_chain` workloads. For each workload, the candidate tier (`128`, `256`, or `512`) must have `path_max_hops_<N>.duration_micros` within the allowed regression budget relative to `path_max_hops.duration_micros` at 64 hops, and it must return useful `path_max_hops_<N>.rows`. Backend-eval must list duration and row metrics for all three candidate tiers across all required workloads so a partial 128-only or deep-chain-only run cannot be mistaken for promotion evidence. On `synthetic_deep_chain`, useful rows must exceed the 64-hop row median so the higher cap proves it exposes deeper paths rather than only preserving parity. Missing full-projection samples, missing row metrics, or latency regressions hold the 64-hop default.

**SQLite frontier guard.** SQLite path and evidence operations are not rewritten unless refreshed backend-eval metrics show they are hot. The guard is metric-backed: full-projection backend-eval must keep `full_projection.sqlite.evidence_target_resolution.duration_micros`, `full_projection.sqlite.evidence.duration_micros`, and `full_projection.sqlite.path_max_hops{,_128,_256,_512}.duration_micros` in the performance gate, and the scan-plan tests must prove single-node and chunked frontier probes use `idx_graph_edges_from_kind` rather than broad edge scans. If those metrics regress, fix the indexed frontier expansion or recursive traversal before changing user-facing hop caps.

**Storage contract.** Each entry in `fixtures/graph-db-performance-history.json` is one backend-eval run and must carry:

- `label` — human-readable description that includes the workload tag (for example `graph-db backend-eval full-projection agent-loop sample 1`).
- `id` — stable identifier of the form `<scope>-<workload>-<date>-sample-<N>`; the trailing `sample-<N>` is the sample index parsed by `perf_gate::parse_history`.
- `timestamp` — ISO-8601 recording time.
- optional run metadata (`workload`, `sample_index`, `backend`, `projection_mode`, `cache_state`, `scope`) may be included to make human review and follow-up triage easier; parsers must continue to treat these fields as additive and derive binding workload/backend/sample facts from `id` plus `metrics`.
- `metrics` — flat map of numeric metrics keyed `<workload_prefix>.<backend_id>.<operation>[.<sub>]`, including:
  - `refresh.duration_micros` for the projection write phase (required for the gate),
  - `total_duration_micros` for the aggregate workload cost (required for the gate),
  - per-operation read metrics (`status`, `edge_lookup`, `edge_property_scan`, `incident_edges`, `path_max_hops*`, `evidence`, `conflict_matrix`, `dispatch_trace`, ...),
  - `duration_micros_per_1k_graph_rows` normalization for any operation,
  - optionally `lock_wait_micros` / `lock_contention_micros` for projection write contention — when present on a candidate, the gate enforces parity-or-better against SQLite.

The fixture is append-only; gate evaluation is read-only. Sibling agents may extend the file with new samples or new workloads between gate runs without breaking the parser. The gate only consumes the four required workload prefixes from any single run; additional informational metrics in the same run are tolerated.

Normal `graph-db refresh`, `conflict-matrix`, and `dispatch-trace` use the same source watermark as backend-eval to reuse the existing projection when the code-index snapshot, agent-doc markdown metadata, and summary cache metadata are unchanged; conflict-matrix preparation additionally keys its reusable packet by source, document, and staged-diff watermarks.

Convex support is a projection backend for the same substrate contract. `GraphProjection::upsert_into` writes nodes before edges, so stores can fail closed when an edge references a missing node. The Convex adapter maps records onto two application tables:

- `nodes`: `externalId`, `kind`, `label`, `properties`, `provenance`, and `freshness`
- `edges`: `edgeKey`, `fromExternalId`, `toExternalId`, `kind`, `properties`, `provenance`, and `freshness`

`externalId` is the stable substrate node id. `edgeKey` is a deterministic key derived from `(from_id, kind, to_id)` so Convex mutations can be idempotent. A Convex schema should index `nodes.by_external_id`, `nodes.by_kind`, `edges.by_edge_key`, `edges.by_from_kind`, and `edges.by_to_kind`; snapshot endpoints should include that index metadata as `indexes` so `graph-db doctor --backend convex-snapshot` can verify it. Query code must reject snapshot edges whose `fromExternalId` or `toExternalId` is absent before trusting derived graph data, reject duplicate node/edge ids, then enforce provenance and freshness checks. `examples/convex-graph` contains the reusable Convex app-side schema, mutations, HTTP action, and indexed snapshot metadata for the `convex-sync --apply` transport. Backend-agnostic parity tests must prove the same projection round-trips through SQLite and the Convex `nodes`/`edges` adapter.

### libsql Backend

libsql is a fork of SQLite maintained by Turso that supports both local file databases and remote connections to Turso-hosted databases. The `LibsqlGraphStore` (feature-gated behind `backend-libsql`, implemented in `src/libsql_backend.rs`) implements the same `GraphStore` trait as `SqliteGraphStore` using the `libsql` crate for storage and connectivity.

**Feature flag.** Enable with `--features backend-libsql`. This pulls in `libsql` and `tokio` (for the async-to-sync bridge).

**Local mode.** `LibsqlGraphStore::open(path)` opens a local libsql database file using the same schema tables and indexes as `SqliteGraphStore` (`graph_nodes`, `graph_edges`, `graph_node_properties`, `graph_edge_properties`, `graph_projection_versions`, `graph_tombstones`). The schema version and foreign key constraints are identical to the SQLite store.

**Remote mode.** `LibsqlGraphStore::open_remote(url, auth_token)` connects to a remote Turso/libsql server. Remote databases use the same schema and `GraphStore` contract, enabling transparent remote graph storage and retrieval.

**Schema compatibility.** The libsql backend reuses the exact same table definitions, indexes, and schema version (`SQLITE_GRAPH_SCHEMA_VERSION`) as `SqliteGraphStore`. A database file created by `SqliteGraphStore` can be opened by `LibsqlGraphStore::open()` and vice versa, since libsql is wire-compatible with SQLite at the file level.

**Async bridge.** The public `libsql::Connection` API is async. `LibsqlGraphStore` owns a `tokio::runtime::Runtime` and bridges async calls through `block_on`, keeping the synchronous `GraphStore` trait contract.

**Pushdown queries.** `LibsqlGraphStore` overrides `edge()`, `incident_edges()`, and `edges_between_nodes()` with indexed SQL queries instead of relying on the default `all_edges()` scan. `edge()` probes `graph_edges` by `edge_key`. `incident_edges()` uses a UNION of two index probes on `from_id` (via `idx_graph_edges_from_kind`) and `to_id` (via `idx_graph_edges_to_kind`), matching the SQLite backend's `sqlite_incident_edges_union_query` pattern. `edges_between_nodes()` creates a temp table (`_edges_between_ids`), inserts candidate node IDs in batches of 450, then filters `graph_edges` with two `EXISTS` subqueries against the temp table — matching the SQLite backend's temp-table + EXISTS pattern and avoiding the O(N×E) default that iterates outgoing edges per node in Rust. Both overrides avoid the O(E) full-table scan that the default `GraphStore` implementations would incur.

**Performance gate eligibility.** The libsql backend is a candidate for the `backend-eval` promotion gate. When benchmarked, its metric prefix in `fixtures/graph-db-performance-history.json` is `libsql` (e.g. `real.libsql.refresh.duration_micros`). The gate compares libsql samples against the SQLite baseline using the same four required workloads and required metrics.

### SurrealDB Backend Spike

The optional `tsift-surrealdb` crate implements `SurrealdbGraphStore` as a feature-gated adapter spike for embedded/file-backed SurrealDB. It is excluded from default workspace membership and only compiles when the root or CLI `backend-surrealdb` feature is enabled, or when the crate is tested directly with `--manifest-path packages/tsift-surrealdb/Cargo.toml`.

**Feature flag.** Enable with `--features backend-surrealdb`. This pulls in the stable SurrealDB Rust SDK 2.x line with `kv-surrealkv` and `kv-mem`, plus a Tokio runtime bridge. Default `cargo build`, `cargo install`, `cargo clippy --workspace`, and `cargo test --workspace` remain SurrealDB-free.

**Backend-eval mode.** With `backend-surrealdb` enabled, the SurrealDB backend-eval candidate writes provider-neutral `ConvexProjectionRows` into an embedded SurrealKV store under `.tsift/backend-eval-cache/surrealdb/<scope>/<dataset>/surrealkv`, then runs the same `GraphStore` operation suite and SQLite parity checks against that file-backed store. Without the feature, `surrealdb` continues to use the dependency-free read-only prototype.

**Hot-read indexes.** `SurrealdbGraphStore` treats SurrealDB/SurrealKV as durable provider-neutral row storage and maintains Rust-side derived edge indexes for the benchmark hot path: stable edge id lookup, ordered edge scans, edge-kind scans, outgoing adjacency, incoming adjacency, and edge-property probes. The adapter overrides `edge`, `sample_edge`, `sample_edge_with_property`, `incident_edges`, `paged_edges`, `paged_incident_edges`, `edges_between_nodes`, and bounded path reads so backend-eval no longer falls back to the default `all_edges()` scan for those operations. These indexes are rebuilt on open/load and kept in sync across upsert, edge delete, node delete, and clear.

**Pushdown decision.** Native SurrealQL query pushdown remains a persistence/parity probe, not the hot-read path. The optional adapter may compare direct SurrealDB row queries against derived-index results in tests, but backend-eval keeps GraphStore operations on Rust-side indexes because the async SurrealDB/SurrealKV boundary is still slower than in-process lookups on the real workload. Do not replace those hot reads with native SurrealQL pushdown unless repeated `backend-eval --full-projection` samples show operation-level wins for real and full-projection datasets. SQLite remains the optimization focus for production GraphStore work; Rust-derived indexes are not expected to improve SQLite's current hot reads because SQLite already pushes edge lookup, property filters, incident scans, scoped edge collection, and bounded frontier traversal into indexed SQL. Add Rust-side SQLite indexes only for a specific measured preparation cache or operation that indexed SQL cannot satisfy.

**Projection load.** `replace_projection_rows()` bulk-replaces SurrealDB rows through one transaction with bound node and edge arrays, preserving deterministic SurrealDB record ids derived from the stable provider-neutral row ids. It then rebuilds the Rust-side node and edge indexes in one pass from the same provider-neutral rows instead of replaying the public `upsert_node` / `upsert_edge` methods row by row. Backend-eval still reports the candidate projection-load timing as part of the backend refresh/load operation so repeated samples can compare this batch path against SQLite's transactional projection refresh.

**2026-06-02 repeated benchmark.** After Rust-side hot-read indexes and batch projection load, three `backend-eval --candidate surrealdb --full-projection --target gval` cache-hit samples still returned `promotion=hold`. Median totals were: real SQLite 2.6 ms vs SurrealDB 573.2 ms, full-projection SQLite 544.2 ms vs SurrealDB 535.0 ms, high-degree SQLite 11.0 ms vs SurrealDB 7.4 ms, and deep-chain SQLite 12.0 ms vs SurrealDB 4.3 ms. Median refresh/load improved for SurrealDB on full-projection rows, SQLite 541.8 ms vs SurrealDB 82.7 ms, but the real workload stayed blocked at SQLite 0.18 ms vs SurrealDB 88.6 ms. SurrealDB remains held because the real workload and operation-level gates still fail on edge lookup, edge-property scan, incident edges, neighborhood, and path tiers, even when aggregate full-projection and synthetic totals are competitive.

**Promotion boundary.** The feature-gated spike proves projection row writes/load and parity plumbing only. Promotion still requires repeated real, full-projection, high-degree, and deep-chain samples plus install portability and multi-process/read-only lock behavior evidence before SurrealDB can move beyond the native-adapter-required hold gate.

`tsift graph-db refresh` and `tsift traverse` both treat the SQLite substrate as the local graph read model. They build file, symbol, route, session, backlog, agent-doc queue job-packet, worker-result, source-handle, bounded worker-context, semantic-entity, and semantic-concept projection rows, replace the local `.tsift/graph.db` `graph_nodes` / `graph_edges` snapshot, then resolve reports back through the `GraphStore` contract. Source-handle rows are deterministic bounded line windows for source-backed traversal nodes; worker-context rows are deterministic bounded handoff scopes linked from each backlog, queued job-packet, and worker-result node to target-specific source windows instead of every visible source window, so `graph-db kind`, `graph-db neighborhood`, `graph-db related`, `graph-db path`, and `graph-db evidence` can validate real agent-loop/tsift session, backlog, job-packet, worker-result, source-handle, semantic seed, and worker-context behavior through the compiled CLI. If no code source window can be inferred for a backlog/job, the projection links that item to a bounded window around the session document line that declared it, preserving auditable evidence for known backlog ids before any worker run. Worker-result rows are parsed from completed/blocked agent-doc response lines and carry status, touched files, expected tests, and follow-up ids before linking back to backlog/job/source handles. `graph-db evidence` also returns reachable `worker_result`, `semantic_concept`, and `semantic_entity` rows and shortest paths when those rows project. When `.tsift/summaries.db` exists, cached LLM extraction rows project `semantic_entity` and `semantic_concept` nodes plus `mentions_entity`, `mentions_concept`, `tagged_concept`, `related_concept`, and `semantic_relation` edges with the same freshness/provenance discipline as code graph rows. Imported or native `.tsift/memory.db` events also project `semantic_concept` rows, so memory retrieval can satisfy semantic readiness even when no LLM summary cache is available. `graph-db related` depends on refreshed semantic rows for phrase similarity, reports the same semantic-effectiveness `readiness` gate as refresh/status, and intentionally reports a `privacy_boundary`: consent, deletion policy, persona policy, and LiveKit or realtime session state belong in an avatar/agent adapter that writes or queries substrate rows, not inside the GraphStore itself. `graph-db --json refresh/status`, `graph-db --json related`, and `context-pack.graph_orchestration` include a separate `readiness` object that fails closed with `reason=summary_cache_empty` and next commands for `tsift summarize --extract <scope>` plus graph refresh until semantic rows are available from summaries or tsift-memory, without conflating graph projection freshness with semantic-effectiveness readiness. Projection rows include per-record freshness plus a `projection_meta` node with `projection_version=tsift-traversal-v1` and a content hash. `tsift context-pack` also materializes its exploration `source_handle` windows, `worker_context` scopes, and relationship refs into the same substrate before returning the handoff packet.

`tsift conflict-matrix --path <session-or-repo> <target...> --json` is the planner-facing parallel dispatch gate. It refreshes the local graph projection, composes `graph-db evidence` for each backlog id/job handle, builds a `context-pack` handoff summary, reads `diff-digest --cached`, and runs cached `impact` before ranking candidate worker scopes. Context-pack exploration rows (`source_handle`, `worker_context`, and relationship refs) are written through one batched SQLite projection transaction, and conflict-matrix now carries a shared preparation summary for the graph snapshot, evidence packet cache, source handles, worker context/results, semantic rows, dispatch-trace snapshot, and cache hit/miss status that will be reused by downstream trace and backend-eval paths. Prepared context/diff/impact and graph/evidence bundles are persisted under `.tsift/conflict-matrix-cache` by source, document, staged-diff, target, depth, limit, backend, and graph-freshness watermarks so repeated CLI runs can skip the preparation hotspot without trusting stale ownership data. `.tsift/conflict-matrix-cache`, other generated `.tsift` artifacts, `.agent-doc` runtime markdown snapshots/baselines, and build outputs are excluded from source watermarks and indexed graph inputs, including stale relative paths already present in an index snapshot, so writing runtime artifacts cannot self-invalidate the next backend-eval or conflict-matrix run. The report flags shared files, symbols, tests, and config/workflow files, treats shared source/config ownership or staged-file overlap as fail-closed, and emits first-class `worker_prompt_packets` with owned files/symbols, read-only context, forbidden files, expected tests, expansion commands, token budgets, semantic ranking explanations, worker-feedback closure summaries, and explicit fail-closed language for parallel agents. Candidate and worker-prompt rows also expose scheduler-ready fields: `parallel_safe`, `blocks`, `blocked_by`, structured `required_context`, `expected_tests`, and `graph_handles` carrying stable target node, evidence packet, worker prompt packet, source-handle, worker-context, semantic-handle, and projection ids. `cross_target_parallel_safe` reports only the inter-target overlap verdict, while `per_target_fail_closed` lists targets that individually lack safe ownership evidence or otherwise fail closed; legacy `can_parallel` remains false unless both signals permit dispatch. Candidate and worker-prompt rows expose `previously_completed: bool`; when a target has completed worker_result evidence but no reachable source handles or owned files, conflict-matrix downgrades that no-ownership signal to an informational warning instead of `per_target_fail_closed` because the item was already dispatched and should not be reactivated solely to rediscover ownership. Candidate `worker_feedback` summarizes completed/blocked worker_result rows, touched files, expected tests, follow-up ids, chronological outcome history, stale expected tests, follow-up debt, closure rank score, and closure rank reasons. Repeated blockage, stale expected tests, follow-up debt, and completed-result no-ownership downgrades are emitted as warnings/read-only context signals and can reorder otherwise equal safe candidates without changing the hard file/symbol/test/config gates. Unsafe shared-file/config/symbol pairs populate `blocks` / `blocked_by` in rank order, and worker-result follow-up debt is projected as an explicit block edge so agent-doc can compute antichains without scraping prose. Semantic concept/entity evidence can improve ranking among otherwise equal safe candidates, but file/symbol/test/config overlap remains the hard dispatch gate. Conflict-matrix and context-pack/session-review surfaces also record graph projection freshness, projection hashes, evidence packet ids, contract versions, conflict-matrix decisions, worker ownership block labels, replay commands, repair commands, and follow-up graph commands so session-level orchestration can audit why a worker split was or was not safe.

Conflict-matrix and dispatch-trace build target-scoped graph snapshots from resolved targets, evidence paths, owned source handles, file/symbol rows for owned files, and bounded target neighborhoods instead of loading every graph node and edge for ordinary planner decisions.

`tsift dispatch-trace --path <session-or-repo> <target...> --format json|html` exports a compact operator review view over the same graph-backed dispatch data. The JSON/HTML trace links backlog, job_packet, worker_result, worker_context, source_handle, semantic, file/symbol/route, evidence packet id, worker-feedback closure summaries, and worker_prompt_packet rows so operators can inspect the route from prompt to owned source windows without replaying separate commands. Dispatch-trace reuses the conflict-matrix evidence packet cache and graph node/edge snapshot for ID collection instead of reopening the same prepared graph rows, and its JSON includes the same shared preparation counts for operator proof. Dispatch-trace preserves conflict-matrix evidence packet ids, replay commands, repair commands, scheduler fields, stable graph handles, and worker-feedback closure controls so a real agent-doc queue run can be replayed from `graph-db refresh` through `evidence`, `conflict-matrix`, `dependency-dag`, and JSON/HTML trace review.

`tsift dependency-dag --path <session-or-repo> <target...> --json` extracts a graph-level dependency DAG for agent-doc backlog work. It refreshes the same GraphStore projection, resolves the selected backlog ids (or the current session backlog when no targets are supplied), and emits `dependency-dag-v1` nodes, edges, topological batches, cycle diagnostics, replay commands, and repair commands. Directed edges come from explicit dependency text such as `depends on #id`, `after #id`, `blocked by #id`, `before #id`, shared file/symbol/test/config ownership, shared semantic concept/entity evidence, and `worker_result` follow-up ids. Explicit and follow-up edges preserve their dependency direction; overlap edges serialize in session source order so topological batches are deterministic. Cycle diagnostics list blocked nodes and the contributing cycle edges so agent-doc can fail closed or ask for a repair before dispatching DAG-shaped batches.

The graph orchestration JSON contract versions are intentionally explicit: `graph-db-evidence-v1`, `worker-prompt-packet-v1`, `conflict-matrix-v1`, `context-pack-graph-orchestration-v1`, `session-review-follow-up-v1`, `dispatch-trace-v1`, and `dependency-dag-v1`. `tsift graph-db --path . --json schema` lists those versions, `fixtures/graph-db-operator-examples/graph-orchestration-contracts.json` contains compact example envelopes for agent-doc consumers, and `fixtures/graph-db-operator-examples/agent-orchestration-acceptance-pack.json` contains a queue-run acceptance pack with job packets, worker results, refresh/status/doctor replay expectations, stale Convex drift and `convex-sync` repair expectations, conflict-matrix output expectations, dependency-dag freshness/topological/cycle expectations, dispatch-trace replay expectations, and context-pack/session-review follow-up commands. The compiled graph conformance suite regenerates the acceptance pack's normalized `graph-db refresh`, `graph-db status`, `graph-db doctor`, `graph-db evidence`, stale Convex `graph-db drift`, stale `convex-sync`, `conflict-matrix`, `dependency-dag`, `dispatch-trace` JSON/HTML, `context-pack`, and `session-review` samples from the real queue fixture and fails CI when the checked-in `regenerated_samples` contract drifts. Evidence packets persist `packet_id` and `projection_hash`; replay fixtures should rerun the emitted `replay_commands` against the same target and fail closed through the emitted `repair_commands` whenever freshness reports `missing` or `stale`. Stale Convex fixtures must also surface `doctor` / `drift` next commands that include both snapshot diff planning and `--remote-snapshot --apply` repair before Convex-backed reads are considered valid.

`tsift convex-sync <path> --json` emits the concrete Convex sync plan for that local graph projection. It produces idempotent node/edge upsert rows, edge-then-node tombstones when a supplied `--snapshot <rows.json>` contains stale remote rows, ordered chunks controlled by `--chunk-size` (default `50`; the demo `upsertEdges` mutation hits the Convex isolate's 99 MiB carry-over budget around chunk size 100, so the default stays safe and operators raise it only against schemas that have optimized their upsert mutations), required Convex index metadata, and retry/partial-failure diagnostics.

The recommended default workflow is **full-graph sync** (no `--scope`): the cursor-paginated `snapshotMeta` / `snapshotNodesPage` / `snapshotEdgesPage` queries shipped in `examples/convex-graph/graph.ts` (`#convexsnapshotscale`) scale `--remote-snapshot` past the legacy `.collect()` ceiling of ~8 192 rows, so one Convex deployment can hold the whole project graph. `snapshotMeta` is intentionally cheap at full scale: it returns required index metadata, page size, and an indexed `projectionHash` for the requested `projectionMetaId`, but it does not count full tables. When that remote hash matches the local projection hash, tsift treats freshness as current without fetching every row page; missing or mismatched hashes still fall back to the paginated row diff. **Scope-bounded sync** (`tsift convex-sync . --scope <name>`) is the supported escape hatch for: (a) reconciling one submodule independently while the rest of the graph is mid-flight; (b) very-large projects where a single Convex deployment is not the operational target; (c) partial-failure recovery where only one scope's chunks need replay. Tooling on top of `convex-sync` should default to the full-graph form and surface `--scope` as a recovery option, not a primary mode. `--remote-snapshot` pulls the current remote projection hash or rows through a configured HTTP action before diffing, using `--endpoint` or `TSIFT_CONVEX_GRAPH_URL` plus an optional bearer token from `--auth-token-env` (default `TSIFT_CONVEX_AUTH_TOKEN`). `--apply` sends the ordered chunks to the same transport with bounded retries; chunk payloads are idempotent by `externalId` and `edgeKey`. `tsift traverse --convex-snapshot <rows.json>`, `tsift context-pack --convex-snapshot <rows.json>`, and `tsift graph-db --backend convex-snapshot --convex-snapshot <rows.json> ...` fail closed when the Convex snapshot's projection metadata or rows trail the local SQLite correctness store. The live Convex acceptance harness runs in the default test suite, exits as a documented no-op unless `TSIFT_LIVE_CONVEX_ACCEPTANCE=1` and `TSIFT_LIVE_CONVEX_GRAPH_URL` are configured, then applies a small temporary projection to a dedicated Convex deployment, pulls the resulting remote snapshot, and runs graph-db node, kind, neighborhood, and path parity against the local SQLite store. Once the dedicated deployment exists, the same test becomes the release/CI gate for remote snapshot parity.

tsift owns the code adapter on top of this substrate. Code symbols become `code_symbol` nodes and call relationships become `calls` edges with line metadata and tsift index provenance. Future adapters can add code comments, routes, markdown sessions, backlog items, LiveKit docs, Orbit topics, questions, answers, visuals, and assets without changing the substrate schema. The boundary rule is: no AST, source-language, Orbit, LiveKit, Convex, FalkorDB, or SurrealDB semantics are required to create or query generic substrate records.

Backend-eval full-projection cache keys use a stable input watermark over indexed symbol/call-edge/route rows and semantic summary rows. That full-projection watermark ignores mtime-only `file_state`, path-only index churn, and unrelated agent-doc session markdown edits when those code/summary graph inputs are unchanged; the bounded real dataset remains responsible for current session evidence. Cache-hit samples must continue to report `full_projection.source_graph_build=0us` and `full_projection.projection_rows=0us`, proving the provider-neutral rows were reused instead of rebuilt.

## Per-Submodule Isolation

Each git submodule gets its own index. Isolation tiers control federation (cross-submodule queries):

| Tier | Behavior | Examples |
|------|----------|----------|
| **Isolated** | Never federated, strict boundary | private-client, production-secrets |
| **Private** | Never federated | mail, resume |
| **Shared** | Federated by default | agent-doc, corky, ctx-core-dev |

Config: `.tsift/config.toml` in workspace root.

```toml
[defaults]
federation = true

[overrides.session-share]
federation = false
isolated = true

[overrides.mail]
federation = false
```

Workspace scope ids default to the submodule leaf name when it is unique. If two submodules share the same trailing directory name, tsift promotes those scopes to their full `.gitmodules` paths (for example `pkg/app/foo`, `vendor/foo`) so `--scope` / `--submodule` selectors and `.tsift/indexes/<scope>/index.db` stay collision-free. To target one duplicate scope in `.tsift/config.toml`, use the quoted full path key such as `[overrides."vendor/foo"]`.

## Multiplicity Model

tsift treats repository multiplicity as an ordered ownership stack rather than a flat set of paths. The precedence is:

1. repository root — the fallback project boundary and shared runtime artifact root
2. git submodule scope — the privacy/federation boundary used by `.tsift/config.toml`
3. Cargo workspace — the Rust build graph boundary declared by `[workspace]`
4. Cargo package/crate — the source, feature, target, dependency, and test ownership boundary declared by `[package]`
5. language package-manager workspace — future npm/pnpm/yarn/Python workspace boundaries
6. generated/runtime scope — `.tsift`, `.agent-doc`, build output, caches, and other generated paths excluded from source watermarks
7. agent-doc session scope — the document, backlog, queue, worker-result, and source-window boundary used for orchestration

Higher-numbered layers refine lower-numbered ownership without overriding isolation. For example, a Cargo package inside an isolated git submodule can be selected and indexed locally, but it is still excluded from federated search unless the enclosing submodule permits federation. Selectors are deterministic and fail closed: `--scope <selector>` first preserves existing git-submodule matching, then accepts Cargo package selectors by package name, normalized crate name (`foo-bar` and `foo_bar`), relative package root, or manifest path. Duplicate package names promote selectors to relative package roots, mirroring duplicate submodule leaf-name handling.

Cargo multiplicity is projected into the provider-neutral graph as `cargo_workspace` and `cargo_package` nodes. Workspace nodes use `contains_package` edges. Package nodes carry package name, normalized crate name, package root, workspace root, features, targets, and dependency metadata; they link to owned source files with `owns_file`, to manifest dependencies with `declares_dependency`, and to direct Rust `use` / `extern crate` references with `uses_crate`. These edges stay separate from ordinary call edges so `conflict-matrix`, `dependency-dag`, `dispatch-trace`, and graph-db evidence can reason about package ownership and cross-crate coupling without pretending package dependencies are call sites.

## Storage Layout

```
.tsift/
  indexes/
    agent-doc/
      index.db        # SQLite: function signatures, types, locations
      embeddings.lance # LanceDB: vector embeddings of signatures
      deps.json        # call graph + import graph
      meta.json        # last indexed commit, language stats
    corky/
      ...
  config.toml
```

## New Subcommands

```bash
tsift index --ast <path>        # tree-sitter AST extraction → index.db
tsift index --check <path>      # report stale files without updating the index
tsift index --check --exit-code # exit 1 if stale files found (for scripting/hooks)
tsift index --check --quiet     # summary only — omit per-file change list
tsift index --prune <path>      # conservative full scan; reserved prune surface until subtree invalidation is sound
tsift graph <path>              # build dependency graph → deps.json
tsift graph --callers <symbol>  # who calls this function?
tsift graph --callees <symbol>  # what does this function call?
tsift communities [--path]      # Louvain community detection over call graph
tsift path <from> <to>          # BFS shortest path between symbols
tsift traverse [node] [--to target] --format json|html # Graphify-style file/symbol/session/backlog traversal graph
tsift convex-sync . --snapshot convex-rows.json --chunk-size 100 --json # dry-run Convex nodes/edges sync plan
tsift convex-sync . --remote-snapshot --apply --endpoint https://... --json # live Convex sync transport
tsift graph-db --path . --json schema # stable provider-neutral graph DB JSON schema
tsift graph-db --path . --json refresh # materialize graph.db and report projection/tombstone operator status
tsift graph-db --path . --json status # inspect projection status without refreshing
tsift graph-db --path . --json compact # inspect post-reconciliation compaction policy
tsift graph-db --path . --json compact --apply # checkpoint WAL and VACUUM graph.db storage
tsift graph-db --path . --json backend-eval --candidate duckdb-duckpgq --candidate falkordb --candidate ladybug --candidate kuzu --candidate surrealdb --target cvxa # evaluate experimental GraphStore backend promotion gates
tsift graph-db --path . --json backend-eval | tsift metric-digest --baseline fixtures/graph-db-performance-history.json # digest graph performance history
tsift graph-db --path . --json node <id> # SQLite graph node lookup
tsift graph-db --path . --json kind backlog --property ref_id=cvxa --limit 5 # paged SQLite graph kind scan
tsift graph-db --path . --json evidence cvxa --depth 3 --limit 8 # backlog/job handoff evidence packet
tsift conflict-matrix --path tasks/software/tsift.md pwcm g6kf --json # parallel worker ownership/conflict report
tsift dispatch-trace --path tasks/software/tsift.md pwcm g6kf --format html # graph-backed dispatch trace
tsift dependency-dag --path tasks/software/tsift.md pwcm g6kf --json # graph-backed dependency DAG and topo batches
tsift graph-db --path . --json neighborhood <id> --depth 2 --edge-kind mentions --property path=tasks/software/tsift.md --limit 20 # bounded subgraph
tsift graph-db --path . --json path <from-id> <to-id> --max-hops 64 # bounded shortest directed path
tsift graph-db --path . --json doctor # validate local graph.db without refreshing
tsift graph-db --backend convex-snapshot --convex-snapshot rows.json --json node <id> # Convex snapshot read
tsift graph-db --backend convex-snapshot --convex-snapshot rows.json --json drift # SQLite vs Convex projection diff
tsift graph-db --backend convex-snapshot --convex-snapshot rows.json --json doctor # validate Convex rows/index metadata
tsift graph-db --path . --json related 'memory retrieval query' # first-party tsift-memory / semantic graph retrieval; use instead of direct claude-mem or /mem-search
tsift memory init . --json # initialize first-party .tsift/memory.db
tsift memory import-claude-mem . --all --apply --json # fallback migration for supported claude-mem rows into tsift-memory with per-table count reconciliation; pending_messages is reported but intentionally excluded; large reports cap event_ids and expose event_ids_total/event_ids_truncated
tsift memory status . --json # reports claude_mem_retirement=hold until full import, graph-db semantic retrieval, memory_retrieval_gate parity eval, and one normal no-direct-claude-mem session cycle are proven; rollback commands remain listed while held
tsift memory capture-agent-doc-closeout . --session-path tasks/software/tsift.md --prompt-target 'do [#id]' --response-summary '<summary>' --commit-hash <sha> --session-check-status clean --json # capture agent-doc closeout events into tsift-memory
tsift --envelope explain <symbol> --budget normal # bounded agent preview
tsift --envelope source-read src/main.rs --start 1 --lines 80 --budget normal # bounded source-file preview with expansion handles
tsift --envelope symbol-read <symbol> --file src/main.rs --budget normal # bounded symbol body packet with child refs and graph/source expansion commands
tsift edit < edits.json         # staged multi-file search/replace batch
tsift --envelope edit-intents --path . --budget normal < intents.json # validate semantic AST edit intents and emit dry-run execution plans
tsift --envelope edit-intents --path . --apply --budget normal < intents.json # apply supported, conflict-free Rust semantic edit intents with formatting and rollback
tsift audit                     # scan installed skills, check health
tsift audit --manifest <file>   # compare against expected skill list
tsift summarize <symbol>        # cached LLM summary for a symbol
tsift summarize --extract <path>  # batch LLM extraction (one-time; relative path resolves against --path, workspace files use the matching scoped index)
tsift summarize --extract --diff  # re-extract only git-changed files within the requested path
tsift diff-digest [path]        # bounded worktree diff digest
tsift diff-digest --cached .    # bounded staged-index diff digest
tsift diff-digest --revision HEAD . # bounded single-revision/history digest
tsift impact [path]             # affected-test candidates from changed files, imports, and graph edges
tsift --envelope context-pack tasks/software/tsift.md --test-input test.log --log-input build.log
tsift test-digest --path . < test.log  # bounded test-output digest from stdin or --input
tsift metric-digest < runs.json  # repeated metric-run digest: deltas, improvements, news-ready table
tsift dci-benchmark --fixture fixtures/dci-search-benchmark.json  # recorded multi-hop DCI search comparison
tsift workflow search          # handle-preserving search/explain/summarize/digest recipe
tsift log-digest --path . < build.log  # bounded verbose-log digest from stdin or --input
tsift session-digest --path . < session.md  # session transcript digest: prompt targets, commands, touched files/symbols, failures, closeout
tsift session-cost < session.jsonl  # token/runtime cost digest: prompt totals, cache ratios, large-turn outliers, restart churn
tsift --envelope session-review tasks/software/tsift.md --budget normal
tsift --envelope session-review --next-context tasks/software/tsift.md --budget normal
tsift search <query>            # lexical by default; gains AST-aware ranking when index exists
tsift search --exact <query>    # literal text lookup via `rg -F`
tsift search --autoindex <query> # explicit compatibility flag: build/rebuild before search
tsift search --scope <submod>   # restrict to one submodule's index + lexical root
tsift status              # auto-fixes stale indexes by default, then reports status
tsift status --no-fix     # skip auto-fix, report status only
tsift index --submodule <submod> # unknown/ambiguous workspace scopes fail closed
tsift search --strategy hybrid  # opt-in to slower hybrid BM25 + vector search
tsift search --timeout 60       # custom timeout in seconds (default: 30, 0 = no timeout)
tsift --compact search <query>  # terse human output across commands
```

`tsift session-cost` reports the cost of one transcript or runtime log. `tsift session-review` discovers the newest bounded set of matched sessions for a document or repo target and keeps two cost scopes separate: `aggregate_cost` / aggregate human fields summarize only those visible matched rows, while `latest_session_cost` reports the first/newest matched session by itself. This prevents a multi-session review from presenting cached historical spend as the active session's token total or largest turn while still preserving the bounded cross-session aggregate for trend review.

`tsift summarize --stats`, `tsift summarize <symbol>`, and `tsift summarize --file <path>` are read-only cache queries: they fail closed when `.tsift/summaries.db` is absent, never create the summary cache as a side effect, and retry against a snapshot copy when a live SQLite lock wedges the cache. In WAL mode that snapshot copy includes the sibling `-wal` / `-shm` sidecars instead of copying only the main `.db` file, so read-only fallbacks keep the same committed live state the writer was using. `--path` first resolves through the nearest ancestor `.tsift` project/workspace root, so nested directories reuse the shared summary cache instead of creating shadow caches; `summarize --file` also normalizes equivalent path spellings back to the canonical root-relative cache key, so `src/lib.rs`, `./src/lib.rs`, nested relative spellings that point at the same file, and absolute paths routed through a symlinked checkout all hit the same cached row. Summary cache rows store that root-relative key with `/` separators even on Windows, and read/delete/currentness checks also tolerate legacy `\` rows until they are rewritten. `summarize --stats` reports stale cached files when the source file is missing, when the live blake3 hash no longer matches the cached `content_hash`, and when a cached key is absolute or lexically escapes the project root (`../...`); those out-of-root cache keys count as stale/corrupt and are never opened from the filesystem. If a cached file still exists but cannot be read during stats collection, tsift counts that row as stale, completes the report, and emits a warning instead of aborting the whole command. During `--extract`, relative extract paths resolve against the caller's `--path` anchor (or that file's parent directory), then canonicalize when possible and otherwise collapse lexical `.` / `..` segments before diff filtering, stale-row pruning, and cache-key derivation, while still reusing the ancestor project's shared summary cache. tsift claims an exclusive sibling `summaries.lock` sidecar before it deletes stale rows, rechecks content hashes, or calls the LLM so concurrent extractors fail fast instead of duplicating API spend. Inside that write lock, extraction goes through a lazily-rs `SummaryCache`: each normalized file key has a Slot that reads a content-hash Cell before loading cached rows, so repeated checks for the same file/hash reuse the Slot, while a changed live `content_hash` invalidates the Slot before deciding whether to call the LLM. `SummaryCache::get_or_extract_file` owns the "current rows or compute then replace" branch so extraction skips already-current files without duplicating DB reads, and only writes replacement rows after a stale/missing Slot causes first access to compute summaries. Full re-extracts prune cached summary rows for files that no longer exist inside the requested extract scope even when that scope is now empty, workspace files resolve symbol context against the matching scoped `index.db`, symbol preload uses exact normalized file-path matches so duplicate `src/lib.rs`-style paths across scopes do not bleed into each other, symbol preload reuses the same busy-timeout plus snapshot fallback path as other read-only index consumers when a live lock is present, and `--diff` includes untracked files within the requested extract scope while deleting cached summary rows for tracked files that were removed from that scope, including the old side of `git mv` renames; on an unborn `HEAD`, `--diff` degrades to untracked-only extraction instead of failing on `git diff ... HEAD`. `tsift status` computes summary coverage against live indexed files only, so stale summary rows for deleted files do not over-report cache coverage, and it surfaces summary-cache recovery diagnostics when it had to degrade off the live database.

`tsift edit` now stages each rewritten file beside its target and only swaps the batch into place after every edit validates and every staged file is ready. If any later swap fails, tsift restores earlier files before returning an error instead of leaving a partially-written batch behind.

## Search Stale Precheck + Timeout

`tsift search` now performs a cheap freshness precheck before it calls the sift engine. If an existing local index is stale, search refreshes it before spending time in the lexical engine; callers that pass `--no-autoindex` fail fast instead.

Default behavior:

- fresh index: search proceeds normally
- stale index: search incrementally refreshes the local or scoped index before running
- missing index: search builds the local or scoped index before running when a concrete index target can be resolved

Opt-in recovery:

- `tsift search --autoindex ...` is kept as an explicit compatibility flag for the default behavior: if the local or scoped index is missing or stale, tsift incrementally builds it before searching
- `tsift search --no-autoindex ...` disables the default refresh and fails closed when an existing index is stale
- if that autoindex pass only loses the coarse `index.lock` race to another live tsift writer, search now degrades instead of failing closed: stale indexes continue with the current read-only index snapshot, missing indexes fall back to exact live-file search, and stderr includes one concise retry hint for fresh symbol/index results after the writer finishes
- `tsift search --scope <submod> --autoindex ...` rebuilds only that submodule's index
- `tsift search --federated --autoindex ...` rebuilds stale/missing federated submodule indexes before aggregating symbol hits, and its lexical/vector/hybrid sift pass only searches the same federated scope roots instead of the whole workspace
- `tsift search --scope <submod> ...` now fails closed when the named submodule does not exist, and reports the available scope ids instead of silently searching the workspace root
- `tsift index --submodule <submod> ...` now fails closed on that same unknown or ambiguous selector set, instead of indexing `root/<submod>` into an unreachable scoped DB
- when duplicate submodules share the same trailing directory name, leaf-name selectors fail closed as ambiguous and the full `.gitmodules` path becomes the required scope id
- `tsift status`, `tsift search`, `tsift index`, `tsift locks`, `graph`, `communities`, `path`, and `explain` now resolve nested input paths against the nearest ancestor project/workspace root (`.tsift/` or workspace `.gitmodules`), so subdirectory invocations reuse the intended project/workspace indexes instead of creating nested `.tsift/index.db` state or inspecting synthetic nested lock files
- when a nested workspace path already falls under exactly one submodule source root, `tsift search`, `tsift locks`, `graph`, `communities`, `path`, and `explain` now infer that scoped index automatically instead of requiring a redundant `--scope <scope>` selector
- workspace roots that only have scoped `.tsift/indexes/<scope>/index.db` files now make plain `tsift search` fail closed until the caller picks `--scope <scope>` or `--federated`, instead of auto-creating a second shared root index layout
- workspace roots that only have scoped `.tsift/indexes/<scope>/index.db` files now make `graph`, `communities`, `path`, and `explain` fail closed until the caller picks `--scope <scope>`, instead of surfacing a misleading missing-root-index error
- `tsift search` symbol-hit reads now reopen `index.db` through the same resilient read-only helper used by other index consumers, so a live SQLite lock that appears after the stale-index precheck still falls back to a snapshot copy instead of bubbling a raw SQLite lock error
- writable index updates now claim an OS-backed exclusive lock on the sibling `index.lock` sidecar first, so concurrent `tsift index` / `tsift search --autoindex` writers fail fast with a tsift-owned error instead of surfacing raw SQLite lock contention or PID-recycling false positives
- lock diagnostics intentionally distinguish the tsift-owned `index.lock` sidecar from SQLite WAL/SHM sidecar state: if `tsift locks` sees no live `index.lock` holder but does see live WAL/SHM sidecars, it recommends checking for a wedged SQLite writer and notes that read-only status/search consumers can keep using WAL-aware snapshot fallback
- direct `tsift index`, `tsift search`, and `tsift status` regression coverage must include this WAL-without-`index.lock` mode so future changes do not regress back to raw `database is locked` failures or misleading rollback-journal-only recovery guidance
- read-only graph queries (`graph`, `communities`, `path`, `explain`) now share the same development-machine freshness chain as search: they resolve the local/scoped index target, incrementally refresh missing or stale indexes before opening `index.db`, and then read through the resilient read-only helper; if a concurrent tsift writer already holds `index.lock` and a prior database exists, they skip the refresh with a concise stderr note and continue against the current read-only snapshot
- agent-doc task paths named after workspace scopes, such as `tasks/software/tsift.md`, resolve to that scope's index for both search and read-only graph queries, so graph-backed agent workflows do not require an extra `--scope tsift` flag
- writable `index.db` opens also set `PRAGMA wal_autocheckpoint=256`, so normal tsift write traffic checkpoints the WAL on an explicit budget instead of leaving it entirely to SQLite defaults
- non-fatal source-read / symbol-extraction / call-extraction failures now emit warnings instead of being silently swallowed, and those warnings are carried in `IndexSummary` for JSON consumers

`tsift search` still wraps the sift engine call in a 30-second timeout (configurable via `--timeout`). Timed searches now run in an internal helper process so a timeout kills the underlying sift work instead of leaving a detached worker thread behind. The timeout remains a backstop for genuinely slow lexical searches or for sessions that reach search without a usable index.

Both the in-process lexical path and the timed `__search-worker` helper now point sift at a stable `.tsift/search-cache` directory under the resolved project/workspace root. That keeps corpus/BM25 artifacts reusable across repeated searches, including scoped and federated queries that execute from subpaths but still belong to the same root-owned `.tsift/` state.

`tsift search --exact` (and `--strategy exact`) bypasses that lexical/index precheck entirely and executes a literal `rg -F` scan instead. Plain `tsift search <query>` also auto-promotes single-token identifier/path-like queries such as `claudescore-3`, `alpha_helper`, `src/main.rs`, and `crate::module` to that exact backend by default. That path keeps rg-style lookups fast, works even when the symbol index is stale or missing, and does not require a shared root `.tsift/index.db` in workspaces that only maintain scoped indexes.

When an exact or otherwise high-hit search returns repeated matches from the same file, the default human output now collapses those repeated line hits into one file-level entry with hit counts first and only a couple of representative snippets after that. That keeps broad literal lookups usable without relying on external line truncation alone.

Because that auto-exact routing closes the main literal-lookup gap after the stable search cache work, tsift still defers any native content/FTS table inside `.tsift/index.db`. Broad prose retrieval remains sift's job; exact content lookups stay on ripgrep unless real usage proves that rg-backed exact search leaves an important gap.

`tsift explain` keeps the full JSON/tabular edge list, but the default human-readable caller/callee sections now collapse dense same-file edge sets into grouped file rows with counts. That reduces token volume for highly-connected symbols while preserving the concrete caller/callee names in the grouped summary.

## Bounded Source-File Reads

`tsift source-read <file>` returns a bounded 1-based line window for source inspection. It is intended for agent workflows that would otherwise re-read whole files after search results or diagnostics. Relative file arguments resolve inside the nearest project/workspace root discovered from `--path`, and paths outside that root fail closed.

The JSON and envelope forms (`tsift --envelope source-read src/main.rs --start 40 --lines 80 --budget normal`) emit:

- a stable `swin-*` window handle for the file/range
- line-numbered preview rows capped by the response budget
- `ssym-*` symbol refs for indexed symbols intersecting the window, each with a `symbol-read` expansion command and optional AST `span` metadata (`span-*` handle, node kind, byte range, body range, parent handle, and child handles)
- cached `sum-*` summary refs for the file when `.tsift/summaries.db` is present, each with a `summarize` expansion command
- explicit `before`, `after`, and full-file expansion commands so the next read can expand incrementally instead of falling back to `cat`/large `sed` windows

The command still returns the source preview when index or summary stores are missing; those enrichment failures are reported as warnings. `--scope` restricts index refs for workspace submodule indexes, and nested paths infer the matching workspace scope when possible.

`tsift symbol-read <symbol>` is the symbol-centered read replacement surface. It resolves the query through the indexed symbols table, optionally scoped by `--file` and `--scope`, then emits:

- a stable `sread-*` handle for the selected symbol
- the symbol signature/range metadata, optional AST `span` metadata, and a token-budgeted body preview
- child `ssym-*` refs discovered inside the selected symbol's AST byte span when available, falling back to indexed lines for older indexes
- cached summary refs for the owning file when available
- expansion commands for the selected source window, whole file, `explain`, caller graph, and callee graph

`source-read` symbol refs now expand to `symbol-read`, while `symbol-read` preserves `explain` and graph commands as secondary expansion links. This makes whole-file `Read` fallback unnecessary for the normal search -> source window -> symbol body navigation path.

`tsift edit-intents` is the semantic write-planning and guarded write-executor surface. It accepts JSON `{ "intents": [...] }` batches with normalized intent kinds `rename_symbol`, `replace_function_body`, `insert_import`, `add_method`, `update_call_signature`, `move_declaration`, and `rewrite_call_sites`. The command resolves symbol/file targets against the current index, reports the current content hash, target line range, and target symbol `span` when the index has AST spans, detects optional `expected_content_hash` conflicts, and emits dry-run plans with bounded diff previews by default. For call-site intents, plans include same-file indexed `call_refs`; Rust rewrites currently fail closed when indexed refs cross the target file. With `--apply`, supported Rust intents (`rename_symbol`, `replace_function_body`, `insert_import`, `rewrite_call_sites`, `update_call_signature`) are composed per file, formatted through `rustfmt --edition 2024` before any source file is swapped, and committed through the same backup/rollback atomic edit path as `tsift edit`. `rewrite_call_sites` replaces indexed Rust call expressions with `replacement`; `update_call_signature` replaces the function signature with `replacement` and requires `call_replacement` when same-file call refs must be rewritten. Conflicts, unsupported intent kinds, unsupported languages, cross-file call refs, missing call replacements, AST validation mismatches, and formatter failures fail closed before mutation.

## Handle-Preserving Search Workflow

`tsift workflow search` prints a composable recipe for agents that need to move from literal lookup to broader retrieval without losing stable handles. The JSON and envelope forms (`tsift --envelope workflow search`, `tsift workflow search --json`) list ordered steps for:

- exact anchors: `tsift --envelope search "<literal>" --exact --path . --budget normal`
- semantic broadening: `tsift --envelope search "<concept>" --strategy hybrid --path . --budget normal`
- graph expansion: `tsift --envelope explain "<symbol>" --path . --budget normal`
- summary reads: `tsift summarize "<symbol>" --path . --json`
- digest expansion: `tsift --envelope context-pack <path> --test-input test.log --log-input build.log --budget normal`

The contract is to keep every emitted handle with its originating command, query, path, and strategy, then use each result's `expand`, `follow_up`, or `resume_commands` field for the next command while citing the parent handle. Search previews preserve `sfam-*` and `shit-*` handles, explain previews preserve `edef-*`, `ecall-*`, and `eces-*` handles, and digest/context-pack outputs preserve artifact and touched-symbol handles across diff, test, log, and session expansions.

When an index is present, the AST symbol-ranking prepass is now bounded: SQLite only pulls exact-name rows and overlapping-tag candidates, orders them by exact/tag overlap, and caps that candidate scan to the requested search `--limit` instead of loading the full `symbols` table into memory first.

## Graph Traversal Handles

`tsift traverse` exposes a Graphify-style traversal graph over indexed files, indexed symbols, agent-doc session documents, backlog items, and queued job packets. It assigns stable handles by node family: `gfil-*` for files, `gsym-*` for symbols, `gses-*` for session artifacts, `gbak-*` for backlog items, and `gjob-*` for agent-doc queue entries. Handles are deterministic from normalized file paths, symbol names/locations, session paths, backlog ids, and queue line anchors so agents can cite and revisit graph nodes across turns. The report is now backed by the provider-neutral `GraphStore`: `tsift graph-db refresh` can materialize the traversal projection into `.tsift/graph.db` explicitly for operators, and traversal reads the same typed property graph rows used by the Convex adapter.

The command supports three traversal modes:

- no node: export a bounded graph slice as JSON or HTML (`tsift traverse --path . --format html`)
- node only: return a neighborhood explanation around a handle, symbol name, file path, or backlog id (`tsift traverse '#kgnv' --depth 2 --path .`)
- node plus `--to`: return the shortest path between two graph nodes (`tsift traverse '#kgnv' --to main --path .`)

Traversal edges include file-to-symbol `defines`, symbol-to-symbol `calls`, file-to-route `defines_route`, route-to-symbol `handled_by`, session-to-backlog/job `contains`, job-to-backlog `targets`, and backlog-to-code `mentions` links derived from backlog text tokens. Capped neighborhood traversal scores incident edges before queueing neighbors, so callers, callees, routes, backlog mentions, and semantic rows are selected ahead of lexicographically earlier low-signal nodes. Reports include `recommendations` that rank the next useful graph nodes for bug-fix navigation, prioritizing backlog mentions, shortest-path next hops, routes/handlers, callers/callees, and defining files.

Agent-facing context and traversal packets gate graph evidence on fresh incremental indexes. Before `context-pack` handoffs and `traverse` graph reports read indexed symbols or call edges, tsift checks the matching root or scoped index and runs the normal incremental update path for missing or changed files. Reports include a concise diagnostic when that refresh happened. `context-pack` wraps its trusted handoff pipeline in `InspectScopeGuard`, which is backed by lazily-rs Slots keyed by `(db_path, root, prune)` plus an epoch Cell; repeated `IndexDb::inspect_read_only` calls inside that guarded pipeline share one inspection, and any in-scope index refresh invalidates the Slots before the next read. Changed-file indexing resolves call edges through a lazily-rs `ResolveEdgesCache`: each `(file, content_hash)` gets a Slot whose dependency on an mtime Cell invalidates edge resolution when file metadata changes. Summary extraction uses a lazily-rs `SummaryCache` keyed by normalized file path with a content-hash Cell dependency, so stale/missing summary checks share one DB load per file until the live hash changes and only then compute and replace rows. The default provider-neutral `GraphStore` breadth-first neighborhood traversal also advances through lazily-rs layer Slots, so each evaluated layer depends on the prior layer state and traversal stops without creating deeper layers once the frontier is empty or the requested depth is reached. If a stale or missing index cannot be refreshed, for example because another writer owns the index lock, traversal skips stale symbol/call edges, emits a stale/missing diagnostic, and falls back to live source-file nodes whose expansion commands use `tsift source-read`; this keeps agent-doc navigation grounded in current raw source instead of stale graph evidence.

### lazily-rs Cache Contracts

Every lazily-rs-backed cache must keep its key, lifetime, invalidation dependency, and regression proof explicit. When a new `lazily::Context`, `CellHandle`, or `SlotHandle` is added, update this table and add or extend the matching test before relying on the cache in an agent handoff.

| Surface | Slot key / dependency | Lifetime | Invalidation trigger | Regression proof |
| --- | --- | --- | --- | --- |
| `SummaryCache` (`packages/tsift-summarize`) | Normalized project-relative file key; each slot reads a requested `content_hash` Cell and per-file epoch Cell before loading `.tsift/summaries.db` rows. | One `SummaryCache` instance, currently scoped to an extraction/read cycle under the summary write lock or read-only command path. | A changed live content hash updates the Cell; `get_or_extract_file` replaces rows and calls `invalidate_file`; DB read errors clear the slot. | `summary_cache_reuses_file_snapshot_until_content_hash_changes`; `summary_cache_get_or_extract_file_computes_once_until_hash_changes`. |
| `InspectScopeGuard` / `IndexDb::inspect_read_only` (`packages/tsift-index`) | `(db_path, root, prune)` plus a thread-local scope epoch Cell. | Current thread while the outermost `InspectScopeGuard` is alive; nested guards share the outer Context and drop clears all slots. | `inspect_scope_invalidate_all` after an in-scope index refresh; failed inspections clear their slot. | `inspect_scope_lazily_reuses_until_epoch_invalidation`; `build_context_pack_reuses_inspect_within_scope`; `inspect_read_only_outside_scope_does_not_cache`. |
| `StatusCheckCache` (`packages/tsift-status`) | `StatusInspectKey { db_path, root, prune }` plus a per-status-cycle epoch Cell. | One `tsift status` check/fix cycle, shared by index and summary coverage checks inside that command. | `StatusCheckCache::invalidate_all` after a status-triggered index mutation before the next inspection. | `status_cache_reuses_index_inspection_until_invalidated`. |
| `ResolveEdgesCache` (`packages/tsift-graph`) | `(file, content_hash)` plus a per-slot mtime Cell read by edge-resolution slots. | One changed-file indexing run that owns the `ResolveEdgesCache`. | Updating mtime for the same content hash invalidates the slot; a new content hash creates a new slot. | `resolve_edges_cache_reuses_slots_until_mtime_or_hash_changes`. |
| `GraphStore::neighborhood` (`packages/tsift-core`) | A per-call layer-slot chain seeded by center node, edge-kind filter, and prior layer state. | One neighborhood request; there is no cross-call reuse. | Traversal stops when the frontier is empty or requested depth is reached, and changed options create a new call-local Context. | `graph_store_contract_covers_crud_neighborhood_and_ordering`. |
| `GraphStore::ranked_neighborhood` (`packages/tsift-core`) | A per-call ranked layer-slot chain seeded by center node, scoring options, queue state, seen set, and fetched expansion. | One ranked-neighborhood request; there is no cross-call reuse. | Queue exhaustion, depth, max-node, edge-kind, or scoring-option changes are call-local and rebuild the Context. | `ranked_neighborhood_breadth_first_respects_max_nodes`; `ranked_neighborhood_edge_kind_weighted_prefers_high_score_edges`; `ranked_neighborhood_degree_weighted_prefers_low_degree`. |

Traversal and `context-pack` JSON include an `exploration` packet for agent handoffs. The packet adapts source-window and relationship-map budgets to graph size, exposes compact relationship rows, emits line-numbered `source-read` windows clustered around selected files/symbols/routes, and includes explicit no-reread guidance so follow-up agents expand stable handles instead of replaying whole files. `context-pack` stores those source-window handles as `source_handle` graph nodes and prompt-target scopes as `worker_context` graph nodes so downstream Convex projections can carry the same resumable handles as the local SQLite projection. `tsift traverse --format html` renders the selected GraphStore-backed slice as an inline SVG graph with node filtering, typed coloring, semantic edge styling, and a detail/edge side panel while keeping the JSON packet embedded for offline inspection. Operator examples and stale-snapshot fixtures live under `fixtures/graph-db-operator-examples`.

## Semantic Graph Similarity

`tsift semantic "<query>" --path .` refreshes the local traversal projection, reads semantic nodes through the `GraphStore` contract, and returns the nearest cached concepts/entities by cosine similarity over persisted local embeddings. The query path never calls a model or remote API: model/API usage remains behind explicit `tsift summarize --extract <path>` runs that populate `.tsift/summaries.db`. Semantic graph projection stores a deterministic `embedding_model=tsift-local-hash-v1` and `embedding` property on each `semantic_concept` / `semantic_entity` node so local SQLite, Convex snapshots, and HTML traversal all see the same semantic rows. Graph-db refresh also projects project-local `.tsift/memory.db` events as `memory_session`, `memory_event`, `source_handle`, and `semantic_concept` rows, including events imported from supported `claude-mem` tables and events captured by `tsift memory capture-agent-doc-closeout`; this makes memory retrieval query the first-party store instead of direct `claude-mem` ownership. `claude-mem.pending_messages` is inspected and surfaced in status/import plans, but it is explicitly outside the replacement contract because it contains transient queue state and raw tool payload fields rather than durable memory rows. Use `--kind concept`, `--kind entity`, or `--kind all`, `--limit N`, and `--json` for structured nearest-related-concepts output.

### `impact`

`tsift impact <path>` estimates affected test targets for the current worktree diff, staged diff (`--cached`), or a single revision (`--revision`). It combines changed files and touched symbols from `diff-digest`, import-line scans in test-bearing files, indexed call edges from tests or inline test modules to changed symbols, and route handler references when a changed handler backs a framework route.

The output is advisory, not a proof of test completeness. Each target includes reasons and runnable command suggestions such as `cargo test --test name`, `pytest tests/test_api.py`, or `npm test -- path.spec.ts`.

On stale existing indexes, search exits early with a message like:
```
tsift search aborted: index is stale (51 files). Run `tsift index .` or re-run with `--autoindex`.
```

If the sift engine itself still times out while the search target is fresh, search exits with a non-zero code and prints:
```
tsift search timed out after 30s (strategy: lexical). The search root looks fresh, so reindexing is unlikely to help. Re-run with `--timeout 0` to disable the timeout, narrow `--path` / `--scope`, or try a different strategy.
```

If the timeout re-check finds that the index became stale or disappeared while the worker was running, the timeout error switches back to a concrete rebuild instruction for that target instead of the fresh-root hint.

`--timeout 0` disables the timeout for cases where a long search is expected and keeps the sift call in-process.

## Index Quiet Mode

`tsift index --quiet` (or `-q`) suppresses the per-file change list, printing only the summary line. `--exit-code` implies `--quiet`.

Without `--quiet`, `tsift index --check` on a large repo with 14K+ stale files outputs every file path (1.7MB / 433K tokens in human mode, 2.6MB in JSON). With `--quiet`, output is a single summary line (~80 bytes human, ~120 bytes JSON).

In JSON mode, `--quiet` also omits the `changes` array and uses compact (non-pretty) serialization.

## Global Compact Output

`tsift --compact` is a global flag for human-readable output. It keeps the underlying command behavior the same, but trims verbose formatting across commands:

- `search` drops metadata banners, keeps one-line snippets, reduces score precision, abbreviates kind/match_type labels, uses `syms[N]:` header
- `explain` groups callers/callees by file instead of repeating the same path per edge; abbreviates kind labels; uses `sym:`, `crs[N]:`, `ces[N]:`, `comm[N]:` headers
- `graph` uses `crs[N]:` / `ces[N]:` headers
- `communities` shows top members per cluster with `(+N more)` instead of full dumps; uses `comms n:N e:N iter:N q:Q cnt:N` header with `mbrs` label
- `path`, `status`, `audit`, `summarize`, `diff-digest`, `test-digest`, `log-digest`, `lint`, `sql`, and `index` switch to denser summary-oriented layouts

### Compact Abbreviation Conventions

In `--compact` mode, common labels are shortened:

| Full | Abbreviated | Context |
|------|------------|---------|
| `function` | `fn` | symbol kind |
| `method` | `meth` | symbol kind |
| `class` | `cls` | symbol kind |
| `interface` | `iface` | symbol kind |
| `type_alias` | `type` | symbol kind |
| `data_class` | `data_cls` | symbol kind |
| `sealed_class` | `sealed_cls` | symbol kind |
| `enum_class` | `enum_cls` | symbol kind |
| `companion_object` | `comp_obj` | symbol kind |
| `object` | `obj` | symbol kind |
| `heading` | `h` | symbol kind |
| `code_block` | `code` | symbol kind |
| `exact_name` | `exact` | match type |
| `partial_tags` | `partial` | match type |
| `symbols` | `syms` | section header |
| `callers` | `crs` | section header |
| `callees` | `ces` | section header |
| `community` | `comm` | section header |
| `communities` | `comms` | section header |
| `members` | `mbrs` | section header |
| `symbol` | `sym` | section header |
| `definitions` | `defs` | section header |

Short kinds (`struct`, `trait`, `enum`, `const`, `static`, `mod`, `impl`, `alias`, `union`) pass through unchanged.

`--compact` does not change `--json` formatting. Use `--pretty` for indented JSON.

## Budget-Aware Preview Profiles

`tsift search`, `tsift explain`, `tsift session-review`, and `tsift context-pack` expose preview budgets for agent-facing turns that need bounded follow-up surfaces instead of full prose dumps:

```bash
tsift search "alpha_helper" --budget small
tsift explain alpha_helper --budget normal
tsift session-review tasks/software/tsift.md --budget deep --json
tsift token-savings --fixture fixtures/tsift-token-savings.json --fail-under --json
```

Behavior:

- `--budget <small|normal|deep|auto>` applies named presets. `small` uses 3 items / 120 bytes, `normal` uses 5 items / 160 bytes, and `deep` uses 10 items / 240 bytes.
- `--budget auto` chooses a preset from `TSIFT_CONTEXT_WINDOW`, `CODEX_CONTEXT_WINDOW`, or `CLAUDE_CONTEXT_WINDOW` when one is present: windows at or below 64k use `small`, windows at or above 200k use `deep`, and the fallback is `normal`.
- `tsift --envelope` turns on the adaptive budget by default for these preview-capable commands. Explicit `--budget`, `--max-items`, or `--max-bytes` still wins.
- `--max-items <n>` switches the command into preview mode and caps repeated result groups to `n` items per section.
- `--max-bytes <n>` truncates long preview fields (snippets, messages, paths, labels) to `n` bytes with an ellipsis.
- Preview mode emits deterministic expansion handles plus a concrete follow-up `expand` command for each preview item, so callers can request a narrower rerun without paying for the full original response. Follow-up digest commands use an independent 4-command floor and are not byte-truncated, so `small` remains compact without hiding the `session-review`, `diff-digest`, `test-digest`, and `log-digest` commands needed to resume from a compact handoff.
- Before lexical or hybrid fallback, `tsift search` normalizes free-text agent queries through the `tagpath` query API, so phrases like `get user profile`, `profile user get`, and identifier-shaped terms such as `getUserProfile` resolve against the same canonical symbol-tag stream.
- Symbol-bearing preview items expose a canonical `tag_alias` derived from the `tagpath` family API (for example `alpha/helper`), so search, explain, session-review, and context-pack use one shared family model across surface spellings.
- `context-pack` loads tagpath ontology docs from `.naming/tags/*.md` when present and attaches compact `ontology_refs` to visible symbol refs and summary refs. Each ref carries a stable handle, canonical tag, markdown path, and optional title/domain metadata, while deliberately omitting ontology prose so agents can expand the tag document by path only when needed.
- When search preview mode sees repeated symbol hits that collapse to the same canonical `tag_alias`, it emits one family summary row with match/file counts plus a follow-up `expand` command keyed to that canonical tag family instead of repeating every surface spelling inline.
- When a search preview looks too broad for safe fan-out, the report includes a `scale_guard` with `high-hit` or `corpus-size` level, explicit corpus/tool-budget signals, and concrete `narrow_commands` to run before dispatching parallel agents. Envelope `follow_up` lists those narrowing commands before ordinary item expansion commands.
- JSON/terse/schema output in preview mode returns the same bounded preview report instead of the full raw payload; without these flags, the existing output formats remain unchanged.

`tsift token-savings --fixture <path>` is a CI-friendly report surface for preview compression contracts. The fixture lists per-command cases with raw symbol rows, compact tagpath families, and minimum savings thresholds; session-review cases can include raw `prompt_targets`, `sessions`, `commands`, `touched_files`, `touched_symbols`, `failures`, `guardrails`, and `largest_turns`, context-pack cases can include raw `next_context`, `diff`, `test`, and `log` input rows, and source-read cases can include raw repeated source-file read rows with the compact `source-read` window plus required line anchors. Source-read fixture rows fail closed when a compact window hides a required anchor, so full-file `cat`/`bat` reads and oversized `sed`/`head`/`tail` windows can prove token savings without losing the line references that made the original read useful. That keeps the benchmark focused on the real transcript and handoff sections that dominate prompt volume, not only symbol-family compression. tsift serializes the raw rows and the compact envelope rows, then reports byte deltas, estimated token deltas using `ceil(utf8_bytes / 4)`, savings percentages, and pass/fail status per command. `--json` emits the report as structured data, `--fail-under` exits non-zero when any case misses its fixture threshold, and `tsift --envelope token-savings ...` wraps the same report in the common summary envelope.

`tests/exit_code.rs` runs the compiled `tsift token-savings --fixture ../tagpath/fixtures/tsift-token-savings.json --fail-under --json` path against tagpath's shared fixture and locks the current preview contract to `search`, `explain`, `session-review`, and `context-pack`, including the context-pack fail-under threshold for compact handoff previews. It also runs `fixtures/real-session-token-savings.json`, a tsift-owned benchmark derived from recent tsift/agent-doc transcripts, so `session-review`, `context-pack`, and source-read rewrites keep proving large savings on realistic prompt-target, transcript, diff, test, build, install, push, full-file `cat`/`bat`, and oversized `sed`/`head`/`tail` rows while retaining the resumable follow-up command and required line-anchor surface.

## Structured Envelopes

`tsift --envelope` is a global JSON-mode wrapper for agent-facing preview and handoff commands. It currently applies to `search`, `explain`, `session-review`, and `context-pack`, and it implies `--json`.

Example:

```bash
tsift --envelope search "alpha_helper" --budget small
tsift --envelope session-review tasks/software/tsift.md --next-context --budget normal
tsift --envelope context-pack tasks/software/tsift.md --test-input target/test.log --budget auto
```

Envelope shape:

- `tool`: command name (`search`, `explain`, `session-review`, `context-pack`)
- `view`: report shape such as `preview`, `report`, `next-context`, or `handoff`
- `summary`: terse display payload with `text` plus `metrics[{label,value}]`
- `truncated`: whether the wrapped report is budget-trimmed
- `follow_up`: concrete rerun or expansion commands callers can surface directly
- `report`: the existing command-specific JSON payload

Preview reports keep their item-level `handle` + `expand` fields inside `report`, so clients can render a top summary from the envelope and then request narrower follow-up expansion without falling back to prose-heavy defaults.

Search preview reports may also include `report.scale_guard`. Clients should surface that warning prominently and prefer the guard's `narrow_commands` before launching independent search/explain/summarize work, because those commands encode the result-count, corpus-size, and preview-budget context that made the original query risky.

### Command/Test-Run Envelopes

`tsift --envelope __digest-runner ... --json` now wraps command-execution digests in a summary-first envelope for `test` and `log` runs.

Behavior:

- The outer envelope stays terse (`tool: "digest-runner"`, `view: "test-run"` or `view: "command-run"`) and surfaces only the summary metrics callers need first, such as runner, exit code, failure count, or signal count.
- The inner `report` carries command metadata plus the existing `test-digest` or `log-digest` payload under `digest`. `report.command` remains the caller's original command, `report.executed_command` records the actual command tsift ran, and `report.filter` records delegated compression metadata when present.
- When `rtk` is installed and `rtk rewrite <command>` supports the wrapped command family, digest-runner executes the RTK-filtered command and wraps that compact output in the same tsift envelope/artifact metadata. Unsupported commands or missing RTK fall back to tsift's built-in capture/digest path.
- When captured stdout/stderr is non-empty, tsift persists it under `.tsift/artifacts/` and returns `report.artifact = {handle, path, bytes, lines, expand}`.
- `handle` is stable for the captured transcript body and command identity, so clients can reference the artifact without inlining the raw output into context.
- `expand` is a concrete replay command (`tsift test-digest ... --input <artifact>` or `tsift log-digest ... --input <artifact>`) that rehydrates the bounded digest from the stored artifact only when the caller explicitly wants details.
- Successful/green runs therefore stay summary-first by default: callers can report the pass/build outcome from the envelope and keep the raw transcript behind the artifact handle instead of replaying it into the turn.

## Compact JSON Default

All `--json` output uses compact (single-line) serialization by default. This saves 30-50% of tokens compared to pretty-printed JSON.

`tsift --pretty` is a global flag that switches JSON output to indented (pretty-printed) format for human readability. Without `--pretty`, JSON is compact.

```bash
tsift search "main" --json                # compact JSON (default)
tsift --pretty search "main" --json       # pretty-printed JSON
tsift --pretty explain main --json        # pretty-printed JSON
```

## Terse JSON Mode

`tsift --terse` is a global flag that outputs JSON with abbreviated field names and an inline schema header. It implies `--json` for any command that supports it.

Output format: `{"_s": {<short→long mapping>}, "d": <data with short keys>}`. The `_s` schema only includes keys that appear in the current response.

```bash
tsift --terse search "main"               # terse JSON (implies --json)
tsift --terse explain main                 # terse JSON
tsift --terse --pretty status .            # terse + pretty-printed
```

**Key mappings** (subset — full list in source):

| Long | Short | Long | Short |
|------|-------|------|-------|
| `caller_file` | `cf` | `caller_name` | `cn` |
| `callee_name` | `en` | `call_site_line` | `csl` |
| `name` | `n` | `kind` | `k` |
| `file` | `f` | `line` | `l` |
| `language` | `la` | `score` | `sc` |
| `end_line` | `el` | `match_type` | `mt` |
| `symbol` | `s` | `symbols` | `sy` |
| `callers` | `crs` | `callees` | `ces` |
| `community` | `cm` | `communities` | `cms` |
| `modularity` | `q` | `members` | `m` |
| `hits` | `h` | `snippet` | `sn` |
| `path` | `p` | `definitions` | `df` |

Unknown keys pass through unchanged.

## Ultra-Terse Mode

`tsift --ultra-terse` is a global flag that applies additional token reduction on top of `--terse` (it implies `--terse`). Targeted at agents operating under tight context budgets.

**Transforms applied (on top of terse key abbreviation):**

1. **Graph node/edge property stripping** — removes the `properties` field from objects detected as graph nodes (have `id` + `kind` + `label`) or graph edges (have `from_id` + `to_id` + `kind`), keeping only `id`, `kind`, `label` for nodes and `id`, `from_id`, `to_id`, `kind` for edges.

2. **Snippet truncation** — `snippet`/`sn` string values are truncated to 80 characters with a `...` ellipsis suffix when the original exceeds 80 chars.

3. **Coverage snapshot compaction** — `SearchCoverageSnapshot` objects retain only `mode`, `total_sector_count`, and `dirty_sector_count`. Removes `active_rebuild`, `completed_dirty_sector_count`, `mounted_sector_count`, `rebuilding_sector_count`, `resumed_sector_count`, `reused_sector_count`.

**Expected savings:** ~25-40% token reduction over terse mode for graph-heavy and search-heavy responses.

```bash
tsift --ultra-terse search "main"          # ultra-terse JSON (implies --terse --json)
tsift --ultra-terse explain main            # stripped graph output
tsift --ultra-terse --envelope search "fn"  # envelope + ultra-terse
```

## Tabular Output

`tsift --tabular` is a global flag that outputs repeated structures as TSV (tab-separated values) with a header row. Designed for structured, token-efficient display that agents and scripts can parse without JSON overhead.

**Supported commands:**
- `search` — symbols table (`match_type`, `kind`, `name`, `file`, `line`, `score`) then hits table (`rank`, `path`, `confidence`, `score`)
- `graph` — edges table (`direction`, `name`, `file`, `line`) with `caller`/`callee` in the direction column
- `communities` — table (`id`, `size`, `members`) where members are comma-separated
- `explain` — definition table (`section`, `kind`, `name`, `file`, `line`) then edges table, then community summary

Truncation is indicated by `# (+N more)` comment lines. Sections are separated by blank lines.

```bash
tsift --tabular search "main"              # two TSV tables: symbols + hits
tsift --tabular graph main --callers       # one TSV table: direction name file line
tsift --tabular communities --limit 5      # one TSV table: id size members
tsift --tabular explain main               # definition + edges + community
```

## Schema-Then-Values Mode

`tsift --schema` is a global flag that converts arrays of same-structured objects into a columnar format: column names once, then rows as value arrays. Implies `--json`.

Output format: for an array of objects with keys `[k1, k2, k3]`, produces `{"_c": [k1, k2, k3], "_r": [[v1, v2, v3], ...]}`.

**Rules:**
- Arrays of 2+ objects with identical key sets are converted
- Arrays with 1 element, heterogeneous keys, or non-object elements pass through unchanged
- Applied recursively to nested objects
- Combines with `--terse`: abbreviated field names in `_c`, plus `_s` schema mapping
- Combines with `--pretty` for indented output

```bash
tsift --schema search "main"               # schema-then-values JSON
tsift --schema --terse search "main"       # abbreviated keys + columnar
tsift --schema --pretty explain main       # indented columnar JSON
```

**Example output (`--schema`):**
```json
{"symbols":{"_c":["kind","line","name"],"_r":[["fn",10,"main"],["fn",20,"helper"]]}}
```

**Example output (`--schema --terse`):**
```json
{"_s":{"k":"kind","l":"line","n":"name"},"d":{"sy":{"_c":["k","l","n"],"_r":[["fn",10,"main"],["fn",20,"helper"]]}}}
```

## Relative Paths (Default)

All file paths in output are project-relative by default. The project root is detected via `path.canonicalize()` in each command. Relative paths are shorter and save tokens — tsift's core mission.

`tsift --absolute` is a global flag that switches output to absolute paths for cases where the full filesystem path is needed (e.g., piping to external tools).

```bash
tsift search "main"                        # paths: src/main.rs
tsift --absolute search "main"             # paths: /home/user/project/src/main.rs
tsift explain main                         # file: src/main.rs, caller_file: src/lib.rs
tsift --absolute graph main --callers      # full paths in output
```

**Scope:** applies to all commands that emit file paths — `search`, `graph`, `explain`, `index`, `summarize`, `lint`, and JSON community member context. Human `communities` output still shows compact symbol names by default.

**JSON output:** path-bearing keys (`file`, `path`, `caller_file`, `file_path`) are stripped in both regular and terse JSON. Non-path string values are never modified.

**Database storage:** paths remain absolute in SQLite. Stripping happens at output time only, so `--absolute` is a display toggle, not a data migration.

## Output Caps (`--limit N`)

Per-command output limits prevent large codebases from flooding agent context windows.

| Command | Flag | Default | What it caps |
|---------|------|---------|-------------|
| `graph` | `--limit N` | 20 | Edges per direction (callers, callees) |
| `communities` | `--limit N` | 10 | Number of communities displayed |
| `explain` | `--limit N` | 15 | Callers and callees each |

`--limit 0` disables the cap (show everything). When output is truncated, a `(+N more)` suffix appears in text mode and `truncated: true` + `*_total` fields appear in JSON.

```bash
tsift graph main --limit 5            # max 5 callers + 5 callees
tsift explain main --limit 0          # show all callers/callees
tsift communities --limit 3           # top 3 communities only
```

**JSON truncation fields:** `total` (or `callers_total`/`callees_total` for graph/explain) gives the full count before truncation. `truncated` (or `callers_truncated`/`callees_truncated`) is a boolean.

## Community Detection (Louvain)

`tsift communities` clusters the call graph into architectural subsystems using the Louvain method.

```bash
tsift communities [--path <path>] [--scope <submod>] [--min-size N] [--json]
```

**Algorithm:** multi-level Louvain (greedy modularity optimization with super-node coarsening) over an undirected, deduplicated call graph.
1. **Phase 1 — local moving:** each symbol starts in its own community. For each node, compute modularity gain of moving to each neighbor's community. Move to the best community if gain > 0. Repeat until convergence (no improving moves or 100 iterations).
2. **Phase 2 — coarsening:** collapse communities into super-nodes. Internal edges become weighted self-loops; inter-community edges become weighted edges between super-nodes.
3. **Repeat** phase 1 on the coarsened graph until no further improvement (up to 10 levels).
4. Map final super-node assignments back to original symbols.

**Output:** communities sorted by size (largest first), total modularity Q ∈ [-0.5, 1.0] (higher = stronger community structure), per-community member list and modularity contribution. JSON `CommunityMember` rows always include `name` and may include `file`, `line`, bounded `refs` (`file`, `line`, `role`, `peer`), and `tagpath_handle` when local index/tagpath evidence can resolve them.

**Terse serialization.** `TerseCommunityMember` strips `file`, `line`, and `refs`, keeping only `name` and `tagpath_handle`. `TerseCommunity` wraps a top-N member slice of `TerseCommunityMember` rows plus `id` and `modularity_contribution`. `CommunityResult::to_terse(top_n)` converts the full result into a `TerseCommunityResult` with truncated member lists. On codebases with large communities, terse output achieves ~40–60% JSON savings compared to full `CommunityResult` serialization by eliminating per-member file/line/refs fields and capping the member list.

**`--min-size N` (default 2):** filter out singleton communities (external symbols with no definition in the indexed codebase).

**Cache and diagnostics:** JSON community output includes `community_diagnostics` with `cache_hit`, `edge_count`, `iterations`, `tagpath_state`, `tagpath_readiness`, tagpath annotation counts, `ambiguous_member_count`, and bounded `ambiguous_members` diagnostics. `tagpath_readiness` fails closed with actionable next commands when `tagpath_state=missing` or stale, so consumers can distinguish "community search succeeded" from "stable `tagpath_handle` coverage is not ready." Louvain results are cached under `.tsift/community-cache/<scope>/` by cache version, scope, a cheap index graph watermark derived from indexed source state plus symbol/edge row counts, and tagpath freshness state. Cache hits reuse the stored `CommunityResult` without re-reading every call edge or rerunning Louvain. Tagpath annotation is bounded to the displayed communities (`tsift communities --limit N`) or the focused symbol's community (`tsift explain`) before handles are emitted, so default limited output does not pay to annotate hidden communities.

**Locking:** `tsift communities` is a read-only graph query. It opens the existing `index.db` without acquiring the writer-side `index.lock`, and if a live SQLite lock temporarily blocks reads it retries against a snapshot copy, including WAL sidecars when present, so the command remains available.

**Boundary rule:** `tsift communities` owns deterministic, AST-derived clustering. For LLM-derived semantic groupings (concept clusters, domain labels), use graphify's semantic layer over `tsift graph --json` output.

## Graph Path Queries

### Shortest Path

`tsift path <from> <to>` finds the shortest path between two symbols using BFS over the undirected call graph. Useful for understanding how distant parts of a codebase are connected.

```bash
tsift path cmd_index apply_changes          # show connection chain
tsift path cmd_index apply_changes --json   # structured output
tsift path cmd_index apply_changes --scope sub  # restrict to submodule
```

The graph is treated as undirected — if A calls B, the path A→B and B→A are both valid hops. Returns null/message when no path exists (disconnected components).

### Symbol Explanation

`tsift explain <symbol>` provides full context for a symbol: definitions, callers, callees, and community membership.

```bash
tsift explain main              # full context for 'main'
tsift explain main --json       # structured output
tsift explain main --scope sub  # restrict to submodule
```

Community membership is computed via the same cached Louvain path as `tsift communities` to show which architectural subsystem the symbol belongs to. Structured explain output includes `community_diagnostics` alongside the focused `community`.

## Graph CLI End-to-End Coverage

The graph-oriented CLI surface should stay covered through the compiled binary, not just unit helpers. `tests/exit_code.rs` owns a real temp-project fixture that runs:

- `tsift search --json`
- `tsift graph --json`
- `tsift communities --json`
- `tsift path --json`
- `tsift explain --json`

Keep that fixture aligned with the command output contracts so changes in indexing, graph extraction, or JSON rendering fail in one integration layer before release.

## Release Workflow

tsift release automation is tag-driven:

- `push` of a `vX.Y.Z` tag runs the release workflow
- the workflow fails closed if the tag does not exactly match `Cargo.toml` `package.version`
- release verification includes `cargo clippy --workspace --all-targets -- -D warnings`, `cargo test --workspace`, and `cargo package -p <crate> --locked --allow-dirty --list` for every split Rust crate in dependency order
- successful tagged releases attach prebuilt archives plus `.sha256` checksum files to the matching GitHub Release
- prebuilt binaries are emitted for `x86_64-unknown-linux-gnu`, `aarch64-apple-darwin`, and `x86_64-pc-windows-msvc`; macOS x86_64 users install from crates.io with `cargo install tsift`
- the crates.io publish job is enabled with the `TSIFT_ENABLE_CRATES_PUBLISH=true` repo variable and a `CARGO_REGISTRY_TOKEN` repo secret; it runs `cargo publish -p <crate> --locked --dry-run` immediately before each real publish after earlier dependency crates have landed, skips crate versions that already exist on crates.io so interrupted releases can resume, and retries crates.io rate limits

tsift's default lexical search adapter is maintained in-tree so crates.io publishing does not depend on the upstream git-only `github.com/rupurt/sift` project. The existing crates.io `sift` crate remains a different project and is intentionally not used.

To keep the remaining dependency surface publish-ready, any dependency that uses a local `path` source should also carry the matching crates.io `version` requirement whenever that published crate already exists.

## Skill Audit

`tsift audit` scans Claude Code skill directories for health and drift detection.

```bash
tsift audit                              # scan ~/.claude/skills/
tsift audit --skills-dir /path/to/skills # custom directory
tsift audit --manifest skills.txt        # compare against expected list
tsift audit --json                       # structured output
```

**Scan checks per skill:**
- Directory exists and is readable
- `SKILL.md` present, non-empty, has `description` in frontmatter
- Symlink target resolves (detects broken symlinks)

**Manifest comparison** (`--manifest`): cross-references installed skills against an expected list (one name per line, `#` comments allowed). Reports:
- `missing` — listed in manifest but not installed
- `orphan` — installed but not in manifest

**Duplicate detection:** after scanning, `tsift audit` computes pairwise Jaccard similarity over description word sets (stop words filtered) and reports skill pairs with score ≥ 30%. Output:
- Human-readable: `60%  skill-a / skill-b` followed by both descriptions
- JSON: `similar_pairs` array with `skill_a`, `skill_b`, `score` (0.0–1.0), `desc_a`, `desc_b`
- Pairs sorted descending by score
- Skills without descriptions are skipped

**Usage tracking** (`--usage`): scans Claude Code session history (`.jsonl` files in `~/.claude/projects/*/`) for `Skill` tool invocations. Counts per skill, flags never-used skills. Plugin-namespaced skills (`codex:rescue`) are counted under the base name (`codex`). Output:
- Per-skill `invocation_count` field
- JSON: `usage` array sorted descending by count
- Skills invoked but not installed are included in the usage list

**Cleanup recommendations** (`--cleanup`): combines health, usage, and duplicate data into an actionable prune list. A skill is flagged when any of:
- Health issues (broken symlink, missing/empty SKILL.md, no description)
- Zero invocations across all sessions
- ≥50% Jaccard similarity with another skill

Each recommendation includes estimated token savings (total file bytes / 4). Sorted by token savings descending.

**Report** (`--report <path>`): writes a markdown audit report to the given path. Includes skills table (status, name, description, uses), duplicate pairs, manifest diffs, and cleanup recommendations with total savings estimate. Suitable as a nightly cron target.

```bash
tsift audit --usage                          # show invocation counts
tsift audit --cleanup                        # actionable prune list
tsift audit --report audit.md                # write markdown report
tsift audit --usage --cleanup --report r.md  # all features
```

## Markdown Lint

`tsift lint` detects unannotated concepts in markdown files by cross-referencing plain text against known graph entities (symbols from the AST index, headings, bold terms, backtick terms).

```bash
tsift lint README.md                              # lint with auto-discovered entities
tsift lint README.md --entities-from SPEC.md      # add entities from another doc
tsift lint README.md --index .tsift               # use a specific project index root
tsift lint README.md --index .tsift/indexes       # use a scoped-index directory directly
tsift lint README.md --json                       # structured output
```

**Entity sources:**
- The file being linted (headings, bold, backtick terms ≥4 chars)
- `--entities-from <path>` markdown files (same extraction)
- `--index <dir>` live symbol index discovery (`index.db`, names ≥4 chars) from a project root, `.tsift` directory, `.tsift/indexes`, scope directory, or direct `index.db` path
- Explicit `indexes` directories recurse through nested scope-id paths (for example `indexes/pkg/app/foo/index.db`), so duplicate-leaf workspace exports stay lintable even outside the original workspace root
- When `--index` points at a workspace aggregate target (workspace root, `.tsift`, `.tsift/index.db`, or `.tsift/indexes`), `tsift lint` applies the same federation filter as auto-discovered roots. Private, isolated, and explicitly non-federated scopes are excluded unless the caller points `--index` at that specific scope directory or `index.db`.
- Workspace aggregate discovery only reads `.tsift`-owned indexes; an unrelated repo-root `index.db` is ignored unless the caller passes that file path explicitly.
- Default: the nearest ancestor project root with `.tsift/index.db`, plus only scoped indexes under `.tsift/indexes/**/index.db` whose workspace scope still participates in federation. Private, isolated, or explicitly non-federated scopes are excluded unless the caller points `--index` at them directly.

**Locking:**
- `tsift lint` opens discovered `index.db` files through the shared read-only path with snapshot fallback for live SQLite locks, including WAL sidecars when present, so lint stays available while a live writer has the index locked.

**Detection rules:**
- Skip code blocks, headings, and HTML comments
- Skip already-annotated terms (backtick-wrapped, bold-wrapped, link text, inside inline code)
- Require word boundaries (no partial matches)
- Classify suggestions: `symbol` → backtick, multi-word capitalized → link, other → bold

**Output:**
- Human-readable: `file:line:col: text → suggestion`
- JSON: `annotations` array with `line`, `column`, `text`, `entity`, `kind`, `suggestion`

## Key Design Decision: Graph > Vector for Code

Aider's repo-map research showed graph-ranked retrieval (PageRank over call/import references) outperforms pure vector similarity for code. The approach: extract symbols via tree-sitter, rank by reference count (centrality), embed only top-ranked. This gives best token efficiency.

## Output Contract

Retrieval returns function-level results, not file-level:
- Function signature + file:line location
- 1-hop dependencies (callers/callees)
- 50-200 tokens per result vs. 2000+ for full-file reads

## Multi-Language Architecture

### Distribution: Cargo Feature Flags

Each grammar is a compile-time feature. Default includes all priority languages. Adding a language = grammar crate + feature + query file + `Language` enum variant.

```toml
[features]
default = ["lang-rust", "lang-python", "lang-typescript", "lang-javascript", "lang-kotlin", "lang-zig", "lang-bash", "lang-markdown"]
lang-rust = ["dep:tree-sitter-rust"]
lang-python = ["dep:tree-sitter-python"]
lang-typescript = ["dep:tree-sitter-typescript"]
lang-javascript = ["dep:tree-sitter-javascript"]
lang-kotlin = ["dep:tree-sitter-kotlin"]
lang-zig = ["dep:tree-sitter-zig"]
lang-bash = ["dep:tree-sitter-bash"]
lang-markdown = ["dep:tree-sitter-md"]
all-languages = ["lang-rust", "lang-python", "lang-typescript", "lang-javascript", "lang-kotlin", "lang-zig", "lang-bash", "lang-markdown"]
```

### Grammar Crates

| Language | Crate | Version | Entry Point | Extensions |
|----------|-------|---------|-------------|------------|
| Rust | `tree-sitter-rust` | crates.io | `LANGUAGE` | `.rs` |
| Python | `tree-sitter-python` | crates.io | `LANGUAGE` | `.py`, `.pyi` |
| TypeScript | `tree-sitter-typescript` | crates.io | `LANGUAGE_TYPESCRIPT` | `.ts` |
| TSX | `tree-sitter-typescript` | crates.io | `LANGUAGE_TSX` | `.tsx` |
| JavaScript | `tree-sitter-javascript` | crates.io | `LANGUAGE` | `.js`, `.mjs`, `.cjs` |
| JSX | `tree-sitter-javascript` | crates.io | `LANGUAGE` | `.jsx` |
| Kotlin | `tree-sitter-kotlin-ng` | 1.1.0 | `LANGUAGE` | `.kt`, `.kts` |
| Zig | `tree-sitter-zig` | 1.1.2 | `LANGUAGE` | `.zig` |
| Bash | `tree-sitter-bash` | 0.25.1 | `LANGUAGE` | `.sh`, `.bash`, `.zsh` |
| Markdown | `tree-sitter-md` | 0.5.3 | `LANGUAGE` + `LANGUAGE_INLINE` | `.md`, `.mdx` |

### Language Module Structure

```
src/
  main.rs
  lang/
    mod.rs          # Language enum, extension dispatch, trait definition
    rust.rs         # Rust query patterns + symbol extraction
    python.rs       # Python query patterns
    typescript.rs   # TypeScript + TSX query patterns
    javascript.rs   # JavaScript + JSX query patterns
    kotlin.rs       # Kotlin query patterns
    zig.rs          # Zig query patterns
    bash.rs         # Bash/Zsh/Shell query patterns
    markdown.rs     # Markdown heading/code block extraction
  queries/          # .scm tree-sitter query files (optional — can inline)
```

### Language Enum

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Language {
    #[cfg(feature = "lang-rust")]     Rust,
    #[cfg(feature = "lang-python")]   Python,
    #[cfg(feature = "lang-typescript")] TypeScript,
    #[cfg(feature = "lang-typescript")] Tsx,
    #[cfg(feature = "lang-javascript")] JavaScript,
    #[cfg(feature = "lang-javascript")] Jsx,
    #[cfg(feature = "lang-kotlin")]   Kotlin,
    #[cfg(feature = "lang-zig")]      Zig,
    #[cfg(feature = "lang-bash")]     Bash,
    #[cfg(feature = "lang-markdown")] Markdown,
}

impl Language {
    pub fn from_extension(ext: &str) -> Option<Self> { /* dispatch table */ }
    pub fn tree_sitter_language(&self) -> tree_sitter::Language { /* grammar entry point */ }
    pub fn symbol_query(&self) -> &'static str { /* .scm query for symbol extraction */ }
}
```

### Per-Language Symbol Extraction

| Language | Symbol Types |
|----------|-------------|
| Rust | `fn`, `struct`, `enum`, `trait`, `impl`, `mod`, `type`, `const`, `static` |
| Python | `def`, `async def`, `class`, decorators, module-level assignments |
| TypeScript | `function`, `class`, `interface`, `type`, `enum`, arrow exports |
| TSX | TypeScript symbols + React component detection (JSX elements) |
| JavaScript | `function`, `class`, arrow exports, `module.exports` |
| Kotlin | `fun`, `class`, `interface`, `object`, `data class`, `sealed class`, `enum class`, `companion object` |
| Zig | `fn`, `struct`, `enum`, `union`, `const` |
| Bash | `function`, alias definitions |
| Markdown | headings (h1-h6), fenced code blocks (with language tag) |

### SQLite Schema Update

```sql
CREATE TABLE symbols (
    id INTEGER PRIMARY KEY,
    name TEXT NOT NULL,
    kind TEXT NOT NULL,        -- 'function', 'class', 'trait', etc.
    language TEXT NOT NULL,    -- 'rust', 'python', 'typescript', etc.
    signature TEXT,            -- full signature for hover/display
    file TEXT NOT NULL,
    line INTEGER NOT NULL,
    end_line INTEGER,
    parent_module TEXT,
    visibility TEXT            -- 'public', 'private', etc. (language-dependent)
);
CREATE INDEX idx_symbols_name ON symbols(name);
CREATE INDEX idx_symbols_language ON symbols(language);
CREATE INDEX idx_symbols_file ON symbols(file);

CREATE TABLE route_nodes (
    id INTEGER PRIMARY KEY,
    framework TEXT NOT NULL,    -- 'axum', 'actix', 'express', 'nestjs', 'fastapi', 'flask'
    method TEXT,                -- 'get', 'post', ... or NULL for framework defaults
    route_path TEXT NOT NULL,
    handler_name TEXT NOT NULL,
    file TEXT NOT NULL,
    line INTEGER NOT NULL,
    handler_line INTEGER
);
CREATE INDEX idx_route_nodes_path ON route_nodes(route_path);
CREATE INDEX idx_route_nodes_handler ON route_nodes(handler_name);
CREATE INDEX idx_route_nodes_file ON route_nodes(file);

CREATE TABLE dir_state (
    path TEXT PRIMARY KEY,
    mtime_secs INTEGER NOT NULL,
    mtime_nanos INTEGER NOT NULL
);
```

### Transactional Index Updates

`apply_changes` and `rebuild` wrap all SQLite mutations in a SAVEPOINT. If any insert, delete, metadata read, or directory-state write fails mid-batch, the entire mutation is rolled back. The index stays at its pre-call state instead of landing in a partially-updated mix of old and new symbols.

`rebuild` nests its own SAVEPOINT around the inner `apply_changes` SAVEPOINT. If a rebuild fails after the bulk DELETEs but before the re-index finishes, both layers are rolled back and the prior index contents are preserved.

### Large Repo Optimization: Prune Surface Held in Safe Mode

`tsift index --prune` still exists as the compatibility surface for future large-repo optimizations, but it no longer skips subtrees based on directory mtimes.

Directory mtimes are not a sound invalidation signal for in-place file edits: modifying `src/foo.rs` usually changes the file's mtime without changing the parent directory's mtime. The previous subtree-pruning shortcut could therefore miss real source edits and leave the symbol index stale.

**Current behavior:**
1. `dir_state` still records directory mtimes so the persistence surface remains stable
2. `--prune` runs the same full file-mtime walk as normal incremental indexing
3. `prune_stats` remain populated, but active subtree skipping stays at zero until a sound invalidation model exists

**Contract:** correctness wins over speculative skipping. Re-enable subtree pruning only when tsift can prove a directory fingerprint that detects in-place file edits, not just creates/deletes/renames.

**Output includes pruning stats:**
```
Index (prune-safe): 50000 files tracked
  new: 2  modified: 1  deleted: 0  unchanged: 49997 | pruned: 0 dirs (312 walked, 0 files skipped)
```

### Future Evolution: Dynamic Grammar Loading

When grammar count makes binary size a concern (30+ languages), add runtime plugin loading:

```bash
tsift lang install haskell     # download prebuilt .so/.dylib
tsift lang list                # show installed grammars
tsift lang remove haskell      # cleanup
```

Dynamic grammars use `tree_sitter::Language::from_path()`. The `Language` enum and query files work identically — only the loading mechanism changes. Compiled-in grammars (features) take precedence over dynamic ones.

## Development Phases

1. Add `tree-sitter` core + priority grammar crates behind feature flags
2. Implement `Language` enum with extension dispatch
3. Write `.scm` query patterns for each language (Rust + Python first)
4. Implement `tsift index --ast` — multi-language symbol extraction to SQLite
5. Wire AST index into `tsift search` — symbol-match ranking first, BM25 fallback
6. Add remaining language queries (TypeScript, JavaScript, Kotlin, Zig, Bash, Markdown)
7. `tsift-graph` crate — language-aware graph extraction (`Lang`, `Symbol`, call sites, routes, community detection, path finding, `LanguageExtractor` trait, `LanguageRegistry`, `ComplexityMetrics`)
8. Per-submodule config + isolation tiers

## Init (Project Setup)

`tsift init` ensures the Code Navigation section is present in `AGENTS.md` for Codex-style harnesses and mirrors it into `CLAUDE.md` when that file exists, so local agent sessions prefer envelope previews plus artifact-backed digest surfaces over raw file reads, diffs, and verbose logs.

```bash
tsift init                              # ensure AGENTS.md (and CLAUDE.md if present) in current directory
tsift init <path>                       # inject at <path> (dir or file)
tsift init src/sub/tasks/plan.md        # resolves to submodule root src/sub/
tsift init --codex                      # also inject auto-reindex hook into .codex/hooks.json
tsift init --codex --workspace          # resolve to workspace root + install one workspace hook
tsift init --opencode                   # install .opencode/commands/tsift-*.md shortcuts
opencode plugin opencode-tsift          # install the same shortcuts after the npm package is published
```

### Path Resolution

`tsift init` resolves the target directory before operating:

1. If `<path>` is a file, use its parent directory
2. Run `git rev-parse --show-toplevel` from that directory to find the git root (handles submodules)
3. Fall back to the directory itself if not in a git repo

This means `tsift init src/session-share/tasks/claudescore-3.md` resolves to `src/session-share/` — the submodule root — and initializes there. When the resolved path differs from the input, a `resolved: <input> → <target>` line is printed.

With `--workspace`, `tsift init` first checks `git rev-parse --show-superproject-working-tree`. When invoked inside a submodule, that promotes the target to the parent workspace root before the normal git-root fallback.

### Behavior

1. Adds `.tsift/` to `.gitignore` (creates the file if needed, appends if entry missing, skips if already present)
2. Ensures `AGENTS.md` exists with the section (creates it if needed)
3. If `CLAUDE.md` exists, updates or appends the same section there too
4. If the section already exists (detected by `<!-- tsift:code-navigation -->` markers), updates it in place
5. Idempotent — running twice produces no changes on the second run
6. With `--codex`: merges a `UserPromptSubmit` auto-reindex hook into `.codex/hooks.json` (creates the file and directory if needed, updates stale tsift commands in place, removes duplicate tsift hook entries, idempotent)
7. With `--opencode`: installs marker-owned `.opencode/commands/tsift-*.md` command templates for status, session review, context pack, diff digest, test digest, log digest, and rewrite-run workflows. Existing marker-owned files are updated idempotently; unmanaged same-name command files fail closed instead of being overwritten. The same marker-owned templates ship in the publishable npm `opencode-tsift` package; after it is published, installing it with `opencode plugin opencode-tsift` gives OpenCode users a registry install path that does not require cloning the tsift repository.
8. When the resolved target has `.gitmodules`, the Codex hook automatically uses `tsift index --check --exit-code --workspace <root>` / `tsift index --workspace <root>` so one root hook covers initialized submodules. `--workspace` makes that root resolution explicit from inside a submodule.
9. The injected Code Navigation section explicitly tells harnesses to switch to the owning repo or submodule root before running tsift/build/test commands, so submodule work does not inherit the wider superproject instruction surface by accident.
10. The injected section also steers harnesses toward envelope-backed `search`, `explain`, `session-review`, `context-pack`, and digest-runner artifacts instead of raw transcript replays, `git diff/show/log` patch dumps, or verbose build/test output reads.
11. The injected section tells agents to run the local default suite with `make check`, then check the latest GitHub Actions CI run with `gh run list --workflow CI --limit 1`; deterministic simulation coverage runs in the default suite, and CI failures must be fixed before the work is complete.

The OpenCode command shortcut set is intentionally prompt-template based rather than a background hook: OpenCode already reads project `AGENTS.md`, and the managed commands give operators explicit `/tsift-status`, `/tsift-session-review`, `/tsift-context-pack`, `/tsift-diff-digest`, `/tsift-test-digest`, `/tsift-log-digest`, and `/tsift-rewrite-run` entrypoints that route common workflows through bounded tsift evidence without depending on raw terminal replay.

`packages/opencode-tsift` is the npm distribution for those same templates. The plugin writes the marker-owned command files into the active project's `.opencode/commands/` directory on load and refuses unmanaged same-name conflicts, matching `tsift init --opencode`. Its `opencode-tsift` CLI entrypoint can also refresh the files directly. The package version follows `Cargo.toml`, and release verification checks that the packaged command files exactly match the Rust `tsift init --opencode` output before an npm publish can run.

### Injected Section

```markdown
<!-- tsift:code-navigation v=0.1.42 -->
## Code Navigation

Run `tsift status` at session start from the owning repo root. If the task or file lives under a git submodule (for example `src/tsift/...`), switch to that submodule root first so the harness loads the narrower local instructions and repo state instead of the superproject root. If status prints a `run:` recommendation for stale or missing tsift state, run `tsift status --fix` before relying on tsift results; when the harness cannot perform write commands, ask the user to run the printed command instead. Codex projects can install a prompt-time auto-reindex hook with `tsift init --codex`; OpenCode projects can install per-project tsift command shortcuts with `tsift init --opencode`.

Use the commands listed in its `use:` output:
- `tsift --envelope search <query> --budget normal` — AST-aware hybrid search preview (prefer over grep/rg)
- `tsift --envelope symbol-read <symbol> --budget normal` — token-budgeted symbol body, child refs, and graph/source expansion commands
- `tsift --envelope edit-intents --path . --budget normal` — semantic edit dry-run plans with target spans and indexed call refs; add `--apply` for supported, conflict-free Rust intents with formatting and rollback
- `tsift --envelope explain <symbol> --budget normal` — callers, callees, community preview
- `tsift graph <symbol> --callers` / `--callees` — call graph navigation
- `tsift summarize <symbol>` — cached summary (only when listed in `use:`)
- `tsift workflow search` — ordered exact/search/explain/summarize/digest recipe that preserves result handles across expansions

When a search envelope includes `report.scale_guard`, run one of its `narrow_commands` before dispatching parallel agents. The guard means the original result set or corpus is broad enough that fan-out should start from a narrower cited handle, path, or exact query.

Prefer bounded digest commands over raw transcript, diff, and verbose-log reads:
- `tsift --envelope session-review <path> --next-context --budget normal` or `tsift --envelope context-pack <path> --budget normal` instead of replaying long session docs, JSONL transcripts, or agent-doc runtime logs with `cat`, `tail`, or `sed`.
- `tsift --envelope source-read <file> --path . --start 1 --lines 80 --budget normal` instead of large whole-file source reads. `tsift rewrite` automatically routes full-file `cat`/`bat` reads and oversized `head`/`tail`/`sed -n` source windows to this surface when the file lives inside an indexed tsift project, while leaving small explicit windows and non-source files untouched.
- `tsift diff-digest [path]` (`--cached`, `--revision <rev>`) instead of `git diff`, `git show`, or patch-style `git log`.
- `tsift --envelope __digest-runner --kind test --path . --shell-command '<test command>'` / `tsift --envelope __digest-runner --kind log --path . --shell-command '<build command>'` for noisy test/build/install output, or let the rewrite/hooks create those artifact-backed envelopes for `cargo test`, `pytest`, and verbose cargo commands.
- If RTK is installed, digest-runner delegates supported generic command families through `rtk rewrite` and records the chosen compact filter in `report.filter` while preserving tsift artifact handles.
- Codex, OpenCode, and other harnesses without Claude-style `PreToolUse` hooks should run `tsift rewrite --run '<command>'` before broad `rg`/recursive grep, raw transcript/session/log reads, `git diff`/`git show`/single-patch `git log`, `cargo test`/`pytest`, and cargo build/check/clippy/install commands so the same search, session-digest, diff-digest, and digest-runner rewrites apply manually. OpenCode can install this path as `/tsift-rewrite-run` with `tsift init --opencode`.

For local verification, run `make check` before committing. After local changes, check the latest GitHub Actions CI run with `gh run list --workflow CI --limit 1` and fix any failing tests before calling the work complete.

Only read full source files when tsift results are insufficient.
<!-- /tsift:code-navigation -->
```

The HTML comment markers enable idempotent updates without parsing markdown structure.

### Version Markers

The opening marker embeds the tsift version (`v=X.Y.Z`) that generated it. When tsift is upgraded:

- `tsift status` reports `instructions: stale` and recommends `tsift init`
- `tsift init` detects the older version marker and replaces the section with the current version's content
- Pre-versioned markers (no `v=` attribute) are treated as stale

This ensures agent sessions always use instructions matching the installed binary.
Release-bump regressions are covered through the compiled CLI path: a stale Code Navigation marker from the previous binary version must be rewritten by `tsift status --json`, and the final JSON report must show `instructions.state=current` for the installed version.

## Status (Session Health Check)

 `tsift status` reports index freshness, instruction version, summary cache availability, and a machine-parseable `use:` list so the agent knows which tsift commands are worth calling this session. When the input path is a nested subdirectory, `status` first promotes it to the nearest ancestor that already owns `.tsift/` so the check reuses the existing project/workspace state, but it stops at a nested git root before considering parent `.tsift/` directories and ignores ambient system-temp-root project markers for child temp dirs so unrelated temp or parent workspaces cannot capture a child repo. On workspace roots, it treats scoped indexes under `.tsift/indexes/<scope>/index.db` as the authoritative status surface even if a shared `.tsift/index.db` also exists. If one or more configured workspace scopes are present on disk but their scoped `index.db` files are missing, the CLI auto-builds just those missing scoped indexes before it prints the final status so a partially initialized workspace does not stay stuck at `index: missing` / `stale` after a successful status pass. `tsift status` automatically applies safe local fixes when the index or instructions are stale: refresh stale or missing indexes, rebuild all existing workspace scopes when the workspace index is stale, refresh stale/missing Code Navigation instructions via `tsift init`, and then print the final status. Within one status command, lazily-rs backs a per-cycle inspection cache so repeated status passes reuse index inspection results and summary coverage can reuse tracked file paths without reopening the same SQLite index; any status-triggered index mutation invalidates the cache before the next pass. Use `--no-fix` to skip auto-fix and report raw status. The deprecated `--fix` flag still works but is a no-op since auto-fix is now the default. When status recommends `tsift summarize --extract ...`, that extract scope is derived from the indexed layout: it uses the common indexed root (for example `src/` when every tracked file or scope lives under `src/`) and falls back to `.` when the indexed files span the project root or multiple unrelated workspace roots.

When the index is stale, `status` also emits a lightweight `reminders` list in JSON and a matching human `reminders:` section. The reminder repeats the concrete reindex command, includes the stale-file or missing-scope count, and notes when no summary cache is available so agents know to refresh the index before relying on search/explain/graph and to run `tsift summarize --extract <scope>` after the index is fresh when summary refs are needed.

```bash
tsift status            # auto-fixes stale index/instructions by default, human-readable output
tsift status --json     # structured JSON output (also auto-fixes by default)
tsift status <path>     # check a specific codebase directory
tsift status --no-fix   # skip auto-fix, report raw status
```

### Output

Four sections: index state, instruction version, summary cache state, recommendations.

When everything is available:
```
index: fresh (last indexed 2m ago, 200 files tracked)
instructions: current (v0.1.0)
summaries: 142/200 files cached (71%)
recommendations:
  use: search, explain, graph, summarize
  run: tsift summarize --extract src/  (58 uncached files)
```

When no index exists:
```
index: missing
instructions: missing (run tsift init)
summaries: unavailable (no index)
recommendations:
  use: (none — run tsift index first)
  run: tsift init && tsift index .
```

When a workspace is indexed through scoped DBs only:
```
index: fresh (workspace, 2 scopes, last indexed 2m ago, 200 files tracked)
  scope alpha: fresh (last indexed 2m ago, 120 files tracked)
  scope beta: fresh (last indexed 1m ago, 80 files tracked)
instructions: current (v0.1.0)
summaries: none
recommendations:
  use: search, explain, graph
  run: tsift summarize --extract src/
```

When a workspace is only partially indexed:
```
index: stale (workspace, 1 indexed scope, 1 missing scope, last indexed 2m ago, 120 files tracked, 0 stale)
  scope alpha: fresh (last indexed 2m ago, 120 files tracked)
  scope beta: missing index (/repo/.tsift/indexes/beta/index.db)
instructions: current (v0.1.0)
summaries: none
recommendations:
  use: search, explain, graph
  run: tsift init --workspace && tsift index --workspace .  (1 missing scope)
```

### JSON Schema

```json
{
  "index": {
    "state": "fresh|stale|missing",
    "total_files": N,
    "stale_files": N,
    "last_indexed_secs_ago": N,
    "workspace_scopes": [
      {
        "scope": "alpha",
        "db_path": "/repo/.tsift/indexes/alpha/index.db",
        "total_files": N,
        "stale_files": N,
        "last_indexed_secs_ago": N
      }
    ],
    "missing_scopes": [
      {
        "scope": "beta",
        "db_path": "/repo/.tsift/indexes/beta/index.db"
      }
    ]
  },
  "instructions": { "state": "current|stale|missing", "version": "0.1.0", "found": "0.0.1", "expected": "0.1.0" },
  "summaries": { "state": "available|none|unavailable", "cached_files": N, "total_indexed_files": N, "coverage_pct": N },
  "recommendations": { "use": ["search", "explain", ...], "run": "tsift index ." },
  "reminders": ["index stale: run `tsift index .` before relying on tsift search/explain/graph (8 stale files); no summaries are cached, so run `tsift summarize --extract .` after the index is fresh when summary refs are needed"]
}
```

### Recommendation Logic

When instructions are stale or missing, `tsift init` is prepended to the `run:` recommendation. Workspace roots use `tsift init --workspace` and `tsift index --workspace .` for their rebuild path. Fresh-index summarize recommendations derive the `--extract` target from the indexed layout instead of assuming `src/`.

| Index | Summaries | `use:` | `run:` |
|-------|-----------|--------|--------|
| missing | — | (none) | `tsift index .` |
| stale | — | search, explain, graph | `tsift index .` |
| fresh | none | search, explain, graph | `tsift summarize --extract <common indexed root>` |
| fresh | partial | search, explain, graph, summarize | `tsift summarize --extract <common indexed root>` |
| fresh | complete | search, explain, graph, summarize | (none) |

## Summarize (Cached LLM Analysis)

`tsift summarize` provides token-efficient access to pre-computed LLM analysis. Pay once for extraction, query free thereafter.

```bash
tsift summarize <symbol>            # show cached summary for a symbol
tsift summarize --file <path>       # show cached summary for a file/module
tsift summarize --extract <path>    # run LLM extraction on path (batch; relative path resolves against --path, or that file's parent directory)
tsift summarize --extract --diff    # re-extract only git-changed files within the requested path
tsift summarize --stats             # summary totals, stale-file count, token savings
tsift summarize --json              # structured output
```

### Architecture

```
tsift summarize
├── extract (one-time, per file content hash)
│   ├── reads source + AST symbols from index.db
│   ├── calls Anthropic batch API (haiku for cost; non-2xx responses fail closed before content parsing)
│   ├── replaces each file's cached rows in one SQLite transaction
│   └── stores: entities, relationships, summaries → summaries.db
├── query (instant, local SQLite)
│   ├── by symbol name → summary + relationships + community context
│   ├── by file path → module-level summary + exported entities
│   └── by concept → cross-file entity matches
└── invalidation
    ├── cache key: blake3(file_content) + symbol_name
    ├── --diff mode: only re-extracts tracked changes plus untracked files within the requested extract scope after that scope is canonicalized / lexically normalized, and treats unborn HEAD as untracked-only
    └── stale entries kept readable, marked for re-extraction
```

### Storage Schema (summaries.db)

```sql
CREATE TABLE summaries (
    id INTEGER PRIMARY KEY,
    symbol_name TEXT NOT NULL,
    file_path TEXT NOT NULL,
    content_hash TEXT NOT NULL,      -- blake3 of source file at extraction time
    summary TEXT NOT NULL,           -- 1-3 sentence description
    entities TEXT,                   -- JSON array of extracted entities
    relationships TEXT,              -- JSON array of {from, to, kind}
    concept_labels TEXT,             -- JSON array of domain concepts
    extracted_at TEXT NOT NULL,      -- ISO timestamp
    model TEXT NOT NULL,             -- model used for extraction
    tokens_input INTEGER,           -- tokens consumed during extraction
    tokens_output INTEGER
);
CREATE INDEX idx_summaries_symbol ON summaries(symbol_name);
CREATE INDEX idx_summaries_file ON summaries(file_path);
CREATE INDEX idx_summaries_hash ON summaries(content_hash);
```

### Extraction Protocol

1. Collect target files (from path arg or `--diff` against `git diff --name-only`; unborn HEAD falls back to untracked files only)
2. Claim the coarse `summaries.lock` sidecar so only one extractor mutates a cache at a time
3. For each file, load source + symbols from `index.db`
4. Build extraction prompt: source snippet + symbol list + "extract entities, relationships, 2-sentence summary"
5. Submit via Anthropic batch API (haiku-class model, 50% cost vs synchronous)
6. On batch completion, parse responses and insert/update `summaries.db`
7. Report: files processed, entities found, tokens spent, estimated savings

### Token Savings Model

Without summarize: reading a 500-line file costs ~2000 tokens per context load.
With summarize: loading the cached summary costs ~50-100 tokens. Savings compound across repeated queries in a session.

`--stats` reports: total summaries, cached files, stale files, and estimated tokens saved across sessions.

### Boundary Rule

`tsift summarize` owns cached, pre-computed analysis that's deterministic after extraction. It does NOT:
- Run live LLM calls at query time (extraction is batch-only)
- Generate new analysis on cache miss (returns "not extracted" + suggests `--extract`)
- Own visualization or graph rendering (leave to graphify)

### Configuration

```toml
# .tsift/config.toml
[summarize]
model = "claude-haiku-4-5-20251001"  # extraction model
batch = true                          # use batch API (50% savings)
max_file_tokens = 8000               # skip files larger than this
api_key_env = "ANTHROPIC_API_KEY"    # env var for API key
```

## Diff Digest

`tsift diff-digest [path]` turns worktree, staged, or single-revision diffs into a bounded, code-aware report for agent context.

```bash
tsift diff-digest .        # current repo root
tsift diff-digest --cached . # staged index against HEAD
tsift diff-digest --revision HEAD . # HEAD commit against its first parent
tsift diff-digest --json . # structured output
```

Behavior:

1. In default mode, collect tracked changes from `HEAD` plus untracked files and compare `HEAD` to the working tree. With `--cached`, compare the staged index to `HEAD`. With `--revision <rev>`, compare that single revision to its first parent (or to the empty tree for a root commit).
2. Parse both snapshots directly with tree-sitter when the file language is supported.
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
3. Extract file anchors and symbol-like tokens from the transcript for quick follow-up lookups. Agent-doc runtime-style `file=...` and `path=...` fields count as file anchors even when they do not carry line numbers, but project-root/directory paths that normalize to an empty display path are ignored; timestamped event names plus `event=...`, `pane=...`, and `session=...` fields are retained as structured symbol refs.
4. When `.tsift/summaries.db` already has current rows for anchored files or extracted symbols, include up to two cached summary snippets; otherwise report `missing`, `stale`, or `unavailable` without mutating the cache.

`log-digest` is intentionally transcript-only. It does not execute the underlying command, and it keeps summary enrichment read-only so digesting verbose output never contends with `tsift summarize --extract`.

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
```

Accepted sources:

- Claude JSONL transcripts with assistant `message.usage` payloads
- Codex JSONL transcripts with `event_msg` `token_count` records
- `agent-doc` runtime `.log` files with start/restart/timeout events

Behavior:

1. Read captured transcript/log input from stdin by default, or from `--input <file>`.
2. Auto-detect Claude JSONL, Codex JSONL, or `agent-doc` runtime logs unless `--source claude-jsonl|codex-jsonl|agent-doc-log` forces one parser.
3. Normalize prompt-side totals, cached-input totals, output totals, and largest per-turn outliers so token-heavy sessions can be compared without ad hoc `jq` pipelines. `session-cost` reports one transcript/log at a time; `session-review` keeps its bounded multi-session aggregate separate from the latest matched session's own cost summary so cached resend totals across many sessions are not mistaken for a single-session bill. Codex `token_count` records prefer `info.last_token_usage` when present, because newer rollouts can interleave more than one cumulative `total_token_usage` stream in a single JSONL file; duplicate cumulative snapshots are skipped, and older records without `last_token_usage` fall back to cumulative deltas.
4. For `agent-doc` runtime logs, summarize bounded churn counters such as `fresh_restart`, `continue`, and `auto_trigger_timeout`, including the highest observed `restart_count`.
5. Derive bounded runtime-churn families from `agent-doc` logs so the digest can call out `fresh_restart`, `auto_trigger_timeout`, ctrl-d restart loops, and clean quit-after-eof exits without replaying the full raw event stream. Clean quit-after-eof exits are summarized for context but do not count as restart-loop warnings.
6. Summarize bounded loop clusters for repeated prompt bodies, repeated command bundles, and repeated closeout churn so common restart/retry patterns become explicit instead of hiding inside the top-N command/event lists.
7. Detect repeated source-file read tool calls in Claude/Codex transcripts, including native `Read` blocks and common shell reads such as `sed -n`, `cat`, `bat`, `head`, and `tail`. The report groups repeated reads by file path plus requested range, estimates total and duplicate token spend using a deterministic line-count heuristic, and emits concrete `tsift source-read ... --budget normal` plus `tsift summarize --file ...` follow-up commands so agents can switch to bounded source windows instead of re-reading the same file/range.
8. Emit guardrails when the session shows obvious budget risk: oversized prompt turns, very high cached-input resend ratios, restart-loop churn, or repeated `commit_already_current` no-op closeouts. `max_restart_count` is reported as context on real restart-churn guardrails, but it must not emit a restart-loop warning by itself when churn families such as `fresh_restart`, `auto_trigger_timeout`, or ctrl-d restart loops are absent. For newer `agent-doc` `document_cycle` logs, collapse repeated closeout lines to one occurrence per `(cycle, event)` before counting so retry noise does not swamp the summary. Each guardrail includes actionable compact/restart guidance.

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
2. For document targets, read `agent_doc_session` from frontmatter when present and use the matching `.agent-doc/logs/<session>.log` to learn historic `file=` aliases plus prior `session=` aliases before scanning other harness logs.
3. Discover related Claude sessions under `~/.claude/projects/<cwd-slug>/`, Codex sessions under `~/.codex/sessions/`, and `agent-doc` runtime logs under `<root>/.agent-doc/logs/`.
4. For directory targets, match candidate logs by cwd. For document targets, require a document-specific signal (`agent_doc_session` or a document path alias) before counting a Claude/Codex transcript; when cwd also matches, report it as supporting evidence instead of letting a shared workspace cwd count by itself. Candidate matching should use structured user/tool-input snippets rather than arbitrary transcript stdout so unrelated hook output or command dumps do not overmatch a shared workspace file name. Reuse the existing `session-digest` and `session-cost` parsers to aggregate prompt targets, commands, failures, closeout evidence, token totals, restart churn, and repeated loop clusters into one bounded report, including Codex `last_token_usage` accounting so interleaved cumulative streams do not inflate review-level token totals or largest-turn outliers.
5. Claude/Codex transcript parsing should skip malformed JSONL lines and ignore non-conversation attachment records where possible so one bad line or hook payload does not fail the whole review.
6. Session-review inherits session-digest's instruction-ballast, successful-test-summary, and failure-meta/progress filtering so copied harness docs, passing test output, assistant assessment prose, and CI/status commentary about false-positive failure groups do not dominate prompt/failure hotspots.
7. Session-review also carries forward aggregate session-cost guardrails so document-level reviews warn when token spend is mostly cached resend, restarts are looping, or closeouts are mostly no-ops. The review should also surface repeated prompt bodies, repeated command bundles, repeated closeout churn, and repeated source-file read diagnostics as explicit summaries instead of leaving that repetition buried inside broader aggregates. Repeated file-read diagnostics retain the path/range grouping, duplicate-token estimate, and concrete `tsift source-read` / `tsift summarize --file` follow-up commands from `session-cost`. When the source is an `agent-doc` runtime log, normalize `document_cycle` closeout details to `phase + event` and count them once per cycle so the review reports distinct closeout cycles instead of raw repeated retries.
8. Aggregate token, command, file, failure, guardrail, loop-cluster, and closeout totals over the same bounded newest matched session rows emitted in `sessions`; older matched transcripts are considered for discovery but do not inflate hidden review totals. The JSON report preserves the legacy top-level token fields for compatibility, adds `aggregate_cost` with `scope: "bounded_matched_sessions"`, adds `latest_session_cost` with `scope: "latest_matched_session"`, and includes each session row's own `largest_turn_total_tokens`. Human and compact output label aggregate token fields explicitly and print the latest-session total/largest-turn pair next to them.
9. `--next-context` emits only the bounded resumable handoff pack: active prompt targets, the last verification closeout state, touched files/symbols, unresolved failures, session-level guardrail action rows, prioritized `next_token_actions`, and the next digest commands to run instead of replaying raw session/log history. Guardrail action rows use the `guardrail:<kind>` failure kind so restart-loop, prompt-budget, cached-resend, and no-op closeout warnings stay visible even when no command failure was extracted. When prompt-budget, cached-resend, restart-loop, or no-op closeout guardrails are present, `next_token_actions` maps them in priority order to exact compact, restart, and digest commands; agent-doc markdown targets include `agent-doc compact <file> --commit`, `agent-doc start <file>`, `tsift --envelope session-review <file> --next-context --budget normal`, and `tsift --envelope context-pack <file> --budget normal`. For agent-doc template documents with a live unresolved `agent:exchange` tail after the latest response boundary, prompt targets, touched files/symbols, and unresolved failures come from that tail rather than historical transcript aggregates; freeform live instructions that do not use a `do [#id]` or slash-command shape still count as active prompt targets and still suppress stale historical files/failures. Frontmatter prompt presets, examples, compacted/archive summaries, completed backlog entries, resolved `### Re:` responses, repeated resolved directives, stale/bogus paths from old matched sessions, instruction prose such as `After finalize...`, source snippets, assistant progress or assessment lines discussing failure extraction/classification false positives, and generic unknown-command exits must not reappear as current handoff failures. If no live document tail is available, `session-review` falls back to the bounded aggregate review fields.

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

`context-pack` is intentionally bounded by default: it emits preview-style lists plus counts rather than dumping the full underlying reports, and `--max-items` / `--max-bytes` further tighten the preview envelope for high-token-pressure turns. Its symbol-bearing preview lists keep the raw `touched_symbols` strings for compatibility while also adding compact symbol-ref objects with stable `handle` ids and canonical `tag_alias` values for `next_context`, diff previews, and log symbol references. If tagpath ontology docs exist under `.naming/tags/*.md`, `context-pack` also loads them once and attaches compact `ontology_refs` to matching symbol refs, summary refs, and the top-level pack; those refs carry handle/tag/path metadata so stable domain vocabulary can be referenced without inlining repeated prose definitions. When the underlying diff/test/log digest already found current cached summaries, the corresponding touched file, failure, signal, file-ref, and symbol/tag-alias family rows expose bounded `summary_refs` with stable handles plus `tsift summarize --file ...` or `tsift summarize <symbol>` expansion commands, so resumptions can keep summary context behind handles instead of inlining every cached summary body.

## Hook Integration

### Auto-Reindex (`UserPromptSubmit`)

`tsift index --check --exit-code` enables scripted freshness checks. The `--exit-code` flag makes `--check` exit 1 when stale files exist (new, modified, or deleted since last index) and exit 0 when fresh. Without `--exit-code`, `--check` always exits 0.

**Claude Code hook** (`.claude/settings.json`):

```json
{
  "hooks": {
    "UserPromptSubmit": [
      { "matcher": "", "command": "examples/hooks/tsift-autoindex.sh" }
    ]
  }
}
```

The hook resolves the git root first, then runs `tsift index --check --exit-code <root>` silently on every prompt. If the repo root has `.gitmodules`, it automatically switches to `tsift index --check --exit-code --workspace <root>` so one root hook covers initialized submodules. When the index is stale, it runs the matching `tsift index ...` rebuild command. When the index is fresh, the check completes in ~50ms with no side effects.

### Search Rewrite (`PreToolUse`)

The existing `tsift-rewrite.sh` hook intercepts high-token shell commands and silently rewrites them to lower-context tsift flows:

- `rg ...` / `grep -r ...` → `tsift --envelope search ... --exact --budget normal`
- `git diff`, `git diff --cached`, `git show`, and simple `git log -p -1 ...` history review → `tsift diff-digest ...`
- long transcript reads (`cat`, `bat`, `head -n`, `tail -n`, `sed -n`) over recognized agent-doc markdown sessions, Claude JSONL, Codex JSONL, or `agent-doc` runtime logs → `tsift session-digest ...`, anchored to the transcript's owning repo or submodule root when the file lives under one
- `cargo test ...`, `pytest ...`, `python -m pytest ...` → `tsift --envelope __digest-runner --kind test ...`
- `cargo build ...`, `cargo check ...`, `cargo clippy ...`, `cargo install ...` → `tsift --envelope __digest-runner --kind log ...`

File-listing commands are not search rewrites. `rg --files ...`, `rg --type-list`, and `find ...` pass through so multiple roots, glob/predicate semantics, shell safety, ignore rules, and the original listing behavior are preserved instead of treating a root path as an exact search pattern. In hook/manual `tsift rewrite` protocol terms this is a no-rewrite exit: stdout stays empty, exit status is 1, and stderr carries a bounded reason plus guidance to run the original command unchanged.

Unsupported shell forms are explicit too. Commands with shell metacharacters such as pipes, redirection, or background operators are not rewritten; the no-rewrite response keeps stdout empty, exits 1, and writes a bounded stderr explanation instead of silently failing. In `--run` mode, no-rewrite exits still do not execute the original command; stderr tells the caller to run the original command directly if intended.

The digest-runner path preserves the wrapped command's original exit status while replacing raw stdout/stderr with a summary-first envelope, bounded digest, and persisted transcript artifact, so failing tests/builds still fail closed and green runs do not inline raw logs. When RTK is installed, digest-runner probes `rtk rewrite <command>` and delegates supported generic command families to RTK's compact filters before wrapping the filtered output in tsift's envelope/artifact metadata. See `~/.claude/hooks/tsift-rewrite.sh`.

Harnesses that do not expose Claude-style `PreToolUse` hooks can still reuse the same rewrite path manually via `tsift rewrite --run '<command>'`. This is the explicit low-token path for Codex/OpenCode broad search, raw session/transcript/log reads, git diff/show/log patch review, cargo/pytest tests, and cargo build/check/clippy/install output. In `--run` mode, tsift executes the rewritten command directly instead of only printing it, preserves the rewritten command's exit status, and emits the same envelope search previews and digest-runner artifact envelopes by default.

Global structured-output flags are forwarded into the rewritten tsift command and deduplicated when the rewrite already chose an envelope. That means callers can still layer `--pretty`, `--terse`, or `--schema` onto the default summary-first execution output, for example:

- `tsift --pretty rewrite --run 'cargo test --manifest-path Cargo.toml'`
- `tsift --schema rewrite --run 'cargo build --manifest-path Cargo.toml'`
- `tsift rewrite --run 'cargo install --path . --force'`

Those commands emit the same `digest-runner` JSON envelope that `tsift --envelope __digest-runner ... --json` uses internally, so agent-doc or other harnesses get bounded execution output without depending on shell-hook rewriting. If RTK is available and supports the wrapped command, `report.filter = {tool:"rtk", command:"..."}` identifies the delegated compact filter.

### RTK Output Filtering (`PreToolUse`)

The `tsift-rewrite.sh` hook (phase 2) routes verbose tsift commands through RTK for output capping when RTK is installed. Commands routed: `communities`, `explain`, `graph`, `index`, `search`. Non-verbose commands (`status`, `init`, `route`, `sql`) pass through unchanged.

RTK TOML filters at `~/.config/rtk/filters.toml` define per-command caps:

| Command | Filter | Effect |
|---------|--------|--------|
| `tsift communities` | `max_lines: 80` | Caps member lists (raw: 600+ lines) |
| `tsift explain` | `max_lines: 40` | Caps callee/caller lists |
| `tsift graph` | `max_lines: 50` | Caps edge lists |
| `tsift index` | `max_lines: 30` | Caps file change lists (raw: up to 14K+ lines) |
| `tsift search` | `strip "Strategy:" line, max_lines: 50` | Strips metadata, caps results |

All filters also strip ANSI codes and blank lines. The `--compact` and `--pretty` global flag variants are matched.

**Interaction with `--quiet`:** the `index` filter is a safety net for unqualified `tsift index` calls. When `--quiet` or `--exit-code` is passed, the binary already suppresses verbose output, making the RTK filter a no-op.

Outside the Claude hook path, `tsift rewrite --run '<command>'` provides a built-in fallback for the same bounded-output policy. Structured `--json` / `--terse` / `--schema` / `--tabular` output stays untouched; remaining human-readable passthrough output is capped only for already-tsift verbose commands that do not have an envelope/structured rewrite form.

## Tagpath integration

Since 0.1.47, `tsift search` auto-detects a [tagpath](https://github.com/btakita/tagpath) index at the project root (`.naming/index.json`) and annotates each `SymbolHit` with a stable `tagpath_handle` field (`mem:<sha256[0..16]>`). Handles are content-addressable: ordinary edits that add a sibling member to a family do not change the family handle, so consumers can cite citations across edits.

### Detection

- The adapter walks up from the search path looking for `.naming.toml`. If none is found, the adapter returns `Missing` and tsift falls back to its native AST extraction with no annotation.
- If an index is found, `tagpath::index::check` runs to decide freshness. Fresh indexes are loaded and used; stale indexes log a `tagpath_index_stale: true` diagnostic to stderr and fall back to live extraction (no `tagpath_handle` is emitted).
- The strict-mode flag below converts the stale fallback into a hard error.

### Flags

- `--no-tagpath` — skip the lookup entirely (no annotation, no diagnostic). Useful for benchmarks and for debugging the native extraction path.
- `--tagpath-strict` — fail closed when the index is present but stale. Use this in CI / hook contexts where silent fallback would be a regression.

### `tagpath_handle` semantics

- The field is `Option<String>` and serializes only when present (`#[serde(skip_serializing_if = "Option::is_none")]`). Consumers that already know the field shape can rely on `tagpath_handle` being either `mem:...` or absent.
- Handle derivation lives in tagpath; see [`src/tagpath/SPEC.md` §15](../tagpath/SPEC.md#15-consumer-contract-tsift--agent-doc--external) for the wire and freshness contract.
- When more than one `symbol_info` row shares a name across files (e.g. two `main` definitions across `bin/foo/main.rs` and `bin/bar/main.rs`), surfaces with file or edge evidence probe every candidate row instead of trusting the first `symbol_info` row. This avoids dropping handles when the first-by-`(file, line)` file lives outside the tagpath walk (vendored, generated, or skipped directory), and lets graph/community outputs prefer the candidate backed by local evidence.
- Callee-edge annotation for `tsift graph` and `tsift explain` resolves each edge with its `caller_file` context instead of sharing one handle per callee name. If multiple indexed files define the callee name, tsift prefers a definition in the caller's file, then one whose file shares the caller's Louvain community evidence, and otherwise falls back to the first resolving handle.
- Community member annotation carries bounded edge refs plus selected `file`/`line` context into JSON output. If multiple indexed files define the same member name, tsift prefers a unique tagpath candidate, then edge-file evidence, then community-file evidence. It no longer assigns a tagpath handle by name-only first-row fallback; unresolved or evidence-resolved duplicate members are reported under `community_diagnostics.ambiguous_members`.
- Scoped `communities` and `explain` community annotation use the scope source root as the tagpath adapter project root, so per-submodule `.naming.toml` / `.naming/index.json` files resolve member handles even when the workspace root has no tagpath project.
- `tsift search --scope <name>` and inferred-scope search paths use the scope's `source_root` as the tagpath adapter project root, so a submodule that owns its own `.naming.toml` / `.naming/index.json` resolves `tagpath_handle` even when the workspace root has no tagpath project.
- `tsift search --federated` runs the tagpath annotation pass **per scope** inside `federated_symbol_search`, using each submodule's `source_root` as the adapter project root. Federated workspaces where each submodule owns its own `.naming.toml` and `.naming/index.json` resolve `tagpath_handle` for every per-scope hit; the workspace root usually has no tagpath project of its own and would otherwise produce a uniform `Missing` adapter load. The merged diagnostic reports `loaded=true` when any scope loaded and `stale=true` with the first scope's stale reason when any scope was stale.
- **Stale-index diagnostic surfaces in structured output**: when any `annotate_*_with_tagpath` helper reports `stale=true`, the JSON response from `tsift search`, `tsift path`, `tsift explain`, `tsift graph`, `tsift communities`, and the search/explain budget-mode reports adds a top-level `tagpath_index_stale: true` and `tagpath_stale_reason: <reason>` pair. The existing stderr `tagpath_index_stale: …` log line is preserved. `--no-tagpath` suppresses both the stderr line and the new JSON fields. JSON consumers can decide to re-run with `--tagpath-strict` or trigger a rebuild from the structured response without scraping stderr.

### Watch integration (deferred)

The current adapter is a one-shot loader; it does not subscribe to `tagpath watch`. A follow-up will add an on-demand `tagpath watch --once` refresh and (eventually) a long-running NDJSON subscription for server-mode tsift.

### Module layout

- `src/tagpath_adapter.rs` — `try_load`, `TagpathAdapter`, `LoadResult`, `HandleResolution`. Public so other tsift commands can opt into the same lookup surface as they wire it through.
- `src/main.rs::annotate_hits_with_tagpath` — annotation helper used by `cmd_search_with_budget`.
- `src/index.rs::SymbolHit::tagpath_handle` — the citation field.

## What NOT to build

- Visualization (Mermaid, HTML) — leave to graphify
- Full LSP-level type inference — diminishing returns
- Embedding model hosting — use external API or lightweight local model (all-MiniLM-L6-v2)
- Dynamic grammar loading (until binary size exceeds ~50MB)
- Live LLM calls at query time in `tsift summarize` — extraction is batch-only
