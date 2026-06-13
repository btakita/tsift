# tsift

`tsift` is a token-conscious code search and session digest CLI for coding agents.
It builds a local index, returns compact search/source/symbol envelopes, and turns
noisy logs, tests, diffs, and agent-doc session documents into bounded evidence
that smaller models can use without replaying an entire repository or transcript.

## Install

Install the latest GitHub release:

```sh
curl -fsSL https://raw.githubusercontent.com/btakita/tsift/main/scripts/install.sh | sh
```

Install a specific version or directory:

```sh
TSIFT_VERSION=0.1.42 TSIFT_INSTALL_DIR="$HOME/bin" sh -c "$(curl -fsSL https://raw.githubusercontent.com/btakita/tsift/main/scripts/install.sh)"
```

The installer supports Linux x86_64 and macOS arm64 release assets. It verifies
the downloaded archive with the release SHA-256 file before installing `tsift`
into `$HOME/.local/bin` by default. macOS x86_64 users should install from
crates.io with `cargo install tsift`.

## Quick Start

```sh
tsift status --fix
tsift init --opencode
tsift --envelope search "route dispatch" --budget normal
tsift --envelope source-read src/main.rs --start 1 --lines 120 --budget normal
tsift --envelope symbol-read main --file src/main.rs --budget normal
tsift --envelope edit-intents --path . --budget normal < intents.json
tsift --envelope edit-intents --path . --apply --budget normal < intents.json
tsift diff-digest .
tsift --envelope session-review tasks/software/tsift.md --next-context --budget normal
tsift graph-db --path . --json refresh
tsift graph-db --path . --json status
tsift graph-db --path . --json compact
tsift graph-db --path . --json kind backlog --property ref_id=cvxa --limit 5
tsift graph-db --path . --json evidence cvxa --depth 3 --limit 8
tsift graph-db --path . --json related --kind all "realtime avatar memory"
tsift conflict-matrix --path tasks/software/tsift.md cvxa --json
tsift dependency-dag --path tasks/software/tsift.md cvxa --json
tsift graph-db --path . --json doctor
tsift graph-db --path . --json backend-eval --candidate kuzu
tsift graph-db --path . --json backend-eval | tsift metric-digest --baseline fixtures/graph-db-performance-history.json
tsift traverse --path . --format html > traversal.html
tsift semantic "graph navigation" --path . --kind concept --json
tsift convex-sync . --chunk-size 100 --json
```

For agent-doc projects, run `tsift status` from the repository root at session
start. If `status` recommends a fix, run `tsift status --fix` before depending
on search or digest output. OpenCode users can run `tsift init --opencode` to
install project-local `.opencode/commands/tsift-*.md` shortcuts for status,
session-review, context-pack, diff-digest, test-digest, log-digest, and
rewrite-run workflows; existing same-name command files without tsift ownership
markers are left untouched and reported as conflicts. The same command pack is
packaged as the `opencode-tsift` npm plugin; once it is published to npm,
OpenCode users can run `opencode plugin opencode-tsift` and get the marker-owned
shortcuts without cloning this repository.

Graph DB and Convex operator examples live under
`fixtures/graph-db-operator-examples`; the reusable Convex app-side schema,
mutations, and HTTP action for `tsift convex-sync --apply` live under
`examples/convex-graph`. Use `tsift graph-db refresh` to materialize
`.tsift/graph.db` explicitly, `tsift graph-db status` to inspect projection
version/hash/watermark and tombstone counts without refreshing, and
`tsift graph-db compact` to inspect or apply the guarded post-reconciliation
WAL checkpoint/VACUUM policy before pruning tombstones. Refresh reports include
node, edge, and materialized-property delta counts so unchanged property rows do
not hide write amplification. Use
`tsift graph-db evidence <backlog-id-or-job-handle>` for bounded
worker-context/source-handle/semantic handoff packets. Use `tsift graph-db
related <phrase>` as the memory-search surface: it turns natural-language
concept phrases into semantic concept/entity seeds and expands incident/outgoing
graph neighborhoods around them for knowledge retrieval from projected
`.tsift/memory.db` rows. `tsift memory import-claude-mem . --all --apply
--json` is the fallback migration path for existing claude-mem stores; it
migrates every supported `observations`, `session_summaries`, and
`user_prompts` row into `.tsift/memory.db` and reports table-level count
reconciliation. Large imports cap returned `event_ids` while preserving
`event_ids_total` and `event_ids_truncated` for proof without oversized JSON.
`tsift memory status . --json` includes a `claude_mem_retirement` gate that
keeps direct claude-mem reads available as rollback until full import, graph-db
semantic retrieval, the recorded memory parity eval, and one normal session
cycle without direct claude-mem or `/mem-search` reads are proven.
Use `tsift
conflict-matrix` to rank candidate worker scopes, flag shared
file/symbol/test/config ownership, and emit first-class worker prompt packets
with worker-result feedback, closure ranking controls, expansion commands, token
budgets, and fail-closed ownership blocks before parallel dispatch. Use `tsift
dispatch-trace --format json|html` for a graph-backed operator review view
linking backlog, job packets, worker results, source handles, semantic rows,
evidence packet ids, worker-feedback closure summaries, and worker prompt
packets. Use `tsift dependency-dag` to extract a replayable agent-doc backlog
DAG with explicit dependency edges, shared ownership/semantic overlap edges,
worker-result follow-up edges, topological batches, and cycle diagnostics before
running DAG-shaped dispatch. Those orchestration surfaces include stable
contract versions, evidence packet ids, projection hashes, replay commands, and
repair commands so agent-doc can consume them as JSON rather than parsing prose.
Use `tsift graph-db backend-eval` to compare SQLite against experimental
read-only candidates, including DuckDB/DuckPGQ, FalkorDB, Ladybug, SurrealDB,
and the dependency-free Vela-Engineering/kuzu row prototype, with adaptive
1/64/128/256/512-hop path probes that keep the default cap at 64 until real and
synthetic extended-hop gates pass, an opt-in `--full-projection` dataset, split
conflict-matrix preparation timings/cache keys that report skipped heavy phases
on cache hits, per-operation promotion gates, projection
load/concurrent-writer/install-portability notes, batched context-pack graph
writes for source/worker context rows, compressed full-projection cache footprint
metrics, and a `performance_gate`/`metric-digest` command for bounded regression
review. Enable `--features backend-surrealdb` to replace the SurrealDB
read-only prototype with the excluded optional `tsift-surrealdb` SurrealKV
adapter spike; default `cargo build` and `cargo install` do not pull SurrealDB
into the dependency graph.
All read-only prototype backends remain held until a native production
adapter beats SQLite across the full-projection promotion gate without weakening
install or lock behavior.
Use `tsift graph-db doctor` to validate local `graph.db` and Convex snapshot
metadata before trusting operator handoffs. `tsift traverse --format html`
renders the selected GraphStore slice as
an offline SVG graph, and `tsift semantic` queries cached summary concepts and
entities from the same persisted graph rows without calling an API. The
operator acceptance pack under `fixtures/graph-db-operator-examples` is
regenerated by the compiled conformance suite from a real agent-doc queue
fixture, including refresh/status/doctor/evidence, stale Convex drift and
convex-sync repair planning, conflict-matrix, dispatch-trace, context-pack, and
session-review samples. The
`live_convex_graph_backend_acceptance_applies_and_matches_graph_db_queries` test
runs in the normal suite and is opt-in via `TSIFT_LIVE_CONVEX_ACCEPTANCE=1` plus
`TSIFT_LIVE_CONVEX_GRAPH_URL`; point it at a dedicated Convex deployment because
it applies and reconciles a temporary projection. Without those settings the
test exits as a documented no-op; with them configured, it becomes a remote
snapshot parity gate for graph-db node, kind, neighborhood, and path queries.

## Release Notes

GitHub release assets are built by the `Release` workflow for:

- `x86_64-unknown-linux-gnu`
- `aarch64-apple-darwin`
- `x86_64-pc-windows-msvc`

The crates.io package path lists every split Rust crate package during release
verification, then the tagged publish job runs `cargo publish --locked --dry-run`
immediately before each real publish in dependency order. The release workflow
uses the `TSIFT_ENABLE_CRATES_PUBLISH=true` repository variable to turn on tagged
crates.io publishes and authenticates via OIDC trusted publishing — it exchanges
a GitHub `id-token` for a short-lived crates.io token through
`rust-lang/crates-io-auth-action`, so there is no long-lived `CARGO_REGISTRY_TOKEN`
secret to expire (each crate must have a Trusted Publisher configured on crates.io).
It skips crate versions that already exist on crates.io so interrupted releases
can resume, and retries crates.io rate limits.
The default lexical search adapter is maintained in-tree, so publishing no
longer depends on an upstream git-only `sift` crate.

The OpenCode command pack under `packages/opencode-tsift` is publishable to npm.
The release workflow dry-runs `npm publish --access public`; tagged npm
publishing is enabled with `TSIFT_ENABLE_NPM_PUBLISH=true` and an `NPM_TOKEN`
repository secret.
