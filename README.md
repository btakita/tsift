# tsift

`tsift` is a token-conscious code search and session digest CLI for coding agents.
It builds a local index, returns compact search and source-read envelopes, and
turns noisy logs, tests, diffs, and agent-doc session documents into bounded
evidence that smaller models can use without replaying an entire repository or
transcript.

## Install

Install the latest GitHub release:

```sh
curl -fsSL https://raw.githubusercontent.com/btakita/tsift/main/scripts/install.sh | sh
```

Install a specific version or directory:

```sh
TSIFT_VERSION=0.1.42 TSIFT_INSTALL_DIR="$HOME/bin" sh -c "$(curl -fsSL https://raw.githubusercontent.com/btakita/tsift/main/scripts/install.sh)"
```

The installer supports Linux x86_64, macOS x86_64, and macOS arm64 release
assets. It verifies the downloaded archive with the release SHA-256 file before
installing `tsift` into `$HOME/.local/bin` by default.

## Quick Start

```sh
tsift status --fix
tsift --envelope search "route dispatch" --budget normal
tsift --envelope source-read src/main.rs --start 1 --lines 120 --budget normal
tsift diff-digest .
tsift --envelope session-review tasks/software/tsift.md --next-context --budget normal
tsift graph-db --path . refresh --json
tsift graph-db --path . status --json
tsift graph-db --path . kind backlog --property ref_id=cvxa --limit 5 --json
tsift graph-db --path . evidence cvxa --depth 3 --limit 8 --json
tsift graph-db --path . doctor --json
tsift traverse --path . --format html > traversal.html
tsift semantic "graph navigation" --path . --kind concept --json
tsift convex-sync . --chunk-size 100 --json
```

For agent-doc projects, run `tsift status` from the repository root at session
start. If `status` recommends a fix, run `tsift status --fix` before depending
on search or digest output.

Graph DB and Convex operator examples live under
`fixtures/graph-db-operator-examples`; the reusable Convex app-side schema,
mutations, and HTTP action for `tsift convex-sync --apply` live under
`examples/convex-graph`. Use `tsift graph-db refresh` to materialize
`.tsift/graph.db` explicitly, `tsift graph-db status` to inspect projection
version/hash/watermark and tombstone counts without refreshing, and
`tsift graph-db evidence <backlog-id-or-job-handle>` for bounded
worker-context/source-handle handoff packets. Use `tsift graph-db doctor` to
validate local `graph.db` and Convex snapshot metadata before trusting operator
handoffs. `tsift traverse --format html` renders the selected GraphStore slice as
an offline SVG graph, and `tsift semantic` queries cached summary concepts and
entities from the same persisted graph rows without calling an API. The
ignored `live_convex_graph_backend_acceptance_applies_and_matches_graph_db_queries`
test is opt-in via `TSIFT_LIVE_CONVEX_ACCEPTANCE=1` and
`TSIFT_LIVE_CONVEX_GRAPH_URL`; point it at a dedicated Convex deployment because
it applies and reconciles a temporary projection. CI runs that acceptance path as
a no-op until the dedicated deployment secret is configured, then it becomes a
remote snapshot parity gate for graph-db node, kind, neighborhood, and path
queries.

## Release Notes

GitHub release assets are built by the `Release` workflow for:

- `x86_64-unknown-linux-gnu`
- `x86_64-apple-darwin`
- `aarch64-apple-darwin`
- `x86_64-pc-windows-msvc`

The crates.io package path is still gated by the upstream `sift` git dependency.
`cargo package` and `cargo publish` cannot succeed until that dependency is
available from crates.io under a compatible package name or tsift stops depending
on the git source.
