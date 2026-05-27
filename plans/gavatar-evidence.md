# #gavatar evidence

## Decision

Tsift can serve as the shared knowledge substrate for realtime avatars and
agents if the product/runtime layer stays outside `GraphStore`.

The retrieval shape is:

1. Project code, markdown, conversation, memory, and other adapter records into
   provider-neutral graph nodes and edges.
2. Project cached semantic concept/entity rows with embeddings.
3. Resolve a natural-language phrase to semantic seed nodes.
4. Expand incident plus outgoing graph edges around those seeds.

That means neighborhoods are useful for general knowledge retrieval, but not as
raw text search. They are the second phase after phrase-to-seed resolution.
This lets users ask word-phrase questions instead of relying on camelCase,
snake_case, or tagpath-style identifiers.

## Implementation

- Added `tsift graph-db related <phrase>`.
- The query returns `semantic_related` seed rows and a
  `knowledge_retrieval` block describing the seeded-neighborhood boundary.
- Seed expansion uses both incident and outgoing edges so adapters can link
  into semantic concepts without reversing their edge direction.
- Existing `graph-db neighborhood` remains stable-id ordered and unchanged for
  cursor pagination.

## Boundary

Tsift should own substrate records, freshness, retrieval, and backend parity.

The avatar/agent adapter should own:

- realtime session state
- LiveKit or voice interruption behavior
- persona policy
- user consent and delete semantics
- per-user memory visibility
- final model response orchestration

## Verification

- `cargo test -q graph_db_related_query_uses_semantic_seeds_and_incident_neighborhoods`
- `cargo test -q cli_parses_graph_db_related_query`
- `cargo test -q graph_db_api_queries_sqlite_neighborhood_and_schema`
- `cargo test -q semantic_related_query_uses_persisted_graph_embeddings`
- `cargo test -q graph_db_backend_eval_benchmarks_candidate_stores_against_sqlite`
- `make check`
- `cargo build`
- `cargo install --path .`

Latest GitHub Actions state at closeout time:

- `gh run list --workflow CI --limit 1`: run `26518765816`, success
- Active workflows: `CI`, `Release`; no separate tmux-test workflow was present
