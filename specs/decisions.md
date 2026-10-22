# tsift Spec — File-Backed Decision Manifests

## Authority boundary

Tracked Markdown is the authority for durable project decisions. The tsift
database is a derived retrieval projection and must never be the only copy of a
decision. A compact `decisions.md` manifest owns stable identity, lifecycle,
summary, scope, and the canonical pointer to longer context. ADRs, specs,
issues, or session documents may own the detailed rationale.

`tsift decision lint` validates that authority without writing it. `tsift
decision import` first performs the same validation, then idempotently upserts
the manifest into `.tsift/findings.db`. Import does not rewrite Markdown.

## Manifest schema

The first Markdown table with the following exact columns is the manifest:

```markdown
| id | status | date | decision | source | anchors | supersedes | related |
|---|---|---|---|---|---|---|---|
| dec-0001 | superseded | 2026-01-02 | Keep the old store | docs/adr-1.md | file:src/store.rs | - | - |
| dec-0002 | accepted | 2026-02-03 | Use the new store | docs/adr-2.md#rationale | code_symbol:GraphStore | dec-0001 | dec-0001 |
```

The columns are:

- `id`: an immutable, authored `DecisionId`. It starts with `dec-` and then
  uses lowercase ASCII letters, digits, and single hyphens. It is not derived
  from mutable title, date, source, or anchor text.
- `status`: `proposed`, `accepted`, `superseded`, or `rejected`.
- `date`: a valid calendar date in `YYYY-MM-DD` form.
- `decision`: a non-empty, bounded summary suitable for initial retrieval.
- `source`: optional (`-`) canonical context. Repository-relative files must
  exist and remain inside the project root; `http://` and `https://` sources
  are also allowed. A `#fragment` may identify a section in a local file.
- `anchors`: optional comma- or `<br>`-separated stable anchors. Each is
  explicit: `code_symbol:<name>` or `file:<repo-relative-path>`. File anchors
  must exist. Import also requires code-symbol anchors to resolve in the current
  index and fails with reindex guidance when they do not. Line numbers are not
  durable anchors and are not accepted.
- `supersedes`: optional decision IDs replaced by this decision.
- `related`: optional non-lifecycle relationships to other decision IDs.

Literal table-cell pipes are escaped as `\|`.

## Lifecycle and link validation

Linting fails closed when:

- an ID is invalid or duplicated;
- a status or date is invalid;
- a source or file anchor escapes the repository or names a missing file;
- a relationship is self-referential, duplicated, or targets a missing ID;
- a `proposed` or `rejected` decision claims to supersede another decision;
- a `supersedes` target is not itself marked `superseded`;
- a superseded decision has no replacement or more than one replacement; or
- the supersedes graph contains a cycle.

A previously accepted decision may retain its outgoing `supersedes` links after
it later becomes superseded. This preserves multi-generation history while an
accepted tip remains the active decision.

## Findings-graph projection

Import projects every manifest row to a node with the explicit graph ID
`decision:<DecisionId>`, kind `decision`, and properties for the decision
summary, lifecycle status, date, source, manifest path, and manifest content
hash. Re-importing the same manifest upserts the same node IDs.

The projection writes:

- `concerns` edges to every code/file anchor (or to the source/manifest as a
  fallback anchor);
- a `documents` edge to the canonical source;
- `supersedes` and `relates_to` edges between decision nodes; and
- provenance identifying the manifest path, decision ID, and content hash.

Accepted decisions map to the findings layer's `trusted` retrieval status.
Proposed, superseded, and rejected decisions map to `draft`, so existing hot
path injection prefers only accepted, non-superseded policy. The original
lifecycle remains separately available as `decision_status`.

The initial importer is additive and idempotent. It updates nodes and declared
edges but does not tombstone a decision removed from the Markdown manifest or
delete a relationship removed from a later revision. A future synchronizer may
perform manifest-scoped reconciliation once deletion provenance and rename
semantics are specified; until then, the Markdown manifest remains the source
of truth and `decision lint` is the authoritative validity check.

## CLI contract

```text
tsift decision lint [MANIFEST] [--path ROOT] [--json]
tsift decision import [MANIFEST] [--path ROOT] [--scope NAME] [--json]
```

`MANIFEST` defaults to `decisions.md` and must be repository-relative. Both
commands fail nonzero on malformed schema or lifecycle/link violations. JSON
lint output includes the normalized records; JSON import output includes the
manifest hash, database path, accepted/total counts, and upsert counts.
