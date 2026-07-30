# Spec: Structural Patterns (ast-grep)

Owns: Structural Search, Structural Rewrite (codemod), Pattern Language, Language
Resolution, Preview/Apply Contract, Known Limits.

Related boundaries: [specs/search-navigation.md](search-navigation.md) owns
index-backed retrieval; [specs/ast-edits.md](ast-edits.md) owns symbol-resolved
semantic edit intents. Structural patterns sit between them — pattern-shaped,
not symbol-resolved, and not index-backed.

## Goal

tsift's `search` answers *"where is this concept"* from a token/AST index.
`edit-intents` answers *"change this resolved symbol"*. Neither expresses
*"every place where code has this shape"* — the query a refactor actually starts
from. Structural patterns close that gap with [ast-grep] patterns evaluated
against the same tree-sitter grammars tsift already compiles, and reuse the
matching engine for rewrite so search and codemod cannot drift apart.

[ast-grep]: https://ast-grep.github.io

## Subcommands

| Command | Behavior |
|---------|----------|
| `tsift ast-grep search <PATTERN>` | Report every non-overlapping match with path, 1-based line/column, byte range, matched text, and metavariable captures |
| `tsift ast-grep rewrite <PATTERN> <REWRITE>` | Rewrite matches. **Previews by default**; writes only under `--apply` |
| `tsift ast-grep languages` | List the grammars compiled into this build |

Shared flags: `--path` (repeatable; defaults to `.`), `--lang`, `--no-ignore`,
`--json`, and the standard preview-budget trio `--max-items` / `--max-bytes` /
`--budget`. Envelope output follows
[specs/output-formats.md](output-formats.md) with `tool: "ast-grep"` and
`view: "search" | "rewrite" | "languages"`.

## Pattern language

Patterns are code fragments in the target language, with metavariables:

- `$A` — a single node; its matched text is reported under `captures`.
- `$$$ARGS` — a variadic run of nodes; reported through the match text, not as
  a single capture.

A rewrite template reuses the pattern's metavariables:
`tsift ast-grep rewrite 'foo($A)' 'bar($A)'`.

## Language resolution

`AstGrepLang` is gated per grammar at the type level rather than re-exporting
`ast_grep_language::SupportLang`. `SupportLang` declares every variant
unconditionally, so a variant whose grammar feature is disabled has no usable
parser — handing one to the engine is a latent panic. The gated enum makes an
uncompiled language an up-front resolution failure that names the supported set.

- Language is inferred per file extension unless `--lang` forces one.
- Aliases follow ast-grep's own `impl_aliases!` table, so a pattern written
  against ast-grep's documentation resolves the same way here.
- `.h` resolves to C, matching ripgrep and tree-sitter convention. C++ headers
  also use `.h`; pass `--lang cpp` to force that.

### Indexable vs structural-only

A tsift `lang-*` feature fans out to up to three crates, and the set a language
reaches is what it can actually do:

| Fans out to | Meaning | Languages |
|---|---|---|
| `tsift-astgrep` + `tsift-graph` + `tsift-search` | Indexable **and** structural. Searchable, graphable, and eligible for a semantic-edit executor | rust, python, typescript, javascript, kotlin, bash, markdown |
| `tsift-graph` + `tsift-search` only | Indexable, **not** structurally matchable | zig — `ast-grep-language` ships no Zig grammar |
| `tsift-astgrep` only | **Structural-only**: `ast-grep search`/`rewrite` work, but the language is not indexed, not searchable, and has no semantic-edit executor | c, cpp, csharp, css, dart, elixir, go, haskell, hcl, html, java, json, lua, nix, php, ruby, scala, solidity, swift, yaml |

Structural-only is a deliberate tier, not an oversight. A tree-sitter grammar is
enough to match and rewrite a shape, but indexing additionally needs per-language
tag queries and symbol extraction — real work that has to be justified per
language. Promoting a structural-only language means adding the graph/search
side first; the `structural_rewrite` edit intent then follows for free, because
it resolves its grammar through the executor contract.

Consequence worth stating plainly: a structural-only language can be rewritten
by `tsift ast-grep rewrite --apply` but **not** by `structural_rewrite`, since
that intent needs a registered executor to reparse and validate the result. The
refusal names the languages that do have executors.

Resolution failure is asymmetric by design:

- A file reached by **walking a directory** whose extension resolves to no
  compiled language is **skipped** — a tree contains many file types.
- A file named **explicitly** on `--path` that resolves to no language is an
  **error** naming the supported set — the user asserted intent about that file.

## Preview / apply contract

1. `rewrite` without `--apply` never touches the working tree. The rewritten
   buffer is still returned so a caller can render a diff without re-running.
2. `--apply` is refused under an active preview budget. A capped scan that also
   wrote would land a partial codemod and report it as complete.
3. A truncated scan sets `truncated: true`. A budget-capped result is never
   reported as if it covered the tree.
4. A rewrite whose output is byte-identical to its input is dropped from the
   report (`unchanged`), so an identity or no-op template does not appear as
   completed work.
5. Edits apply back-to-front. Applying front-to-back would invalidate every
   later byte offset by the accumulated length delta.
6. Non-UTF-8 and binary files are skipped rather than failing the run. Files
   over 2 MiB are skipped: structural patterns target source, and a multi-
   megabyte generated file costs more to parse than it can repay.
7. Walking honours `.gitignore` and hidden-file rules unless `--no-ignore`.

## Edit-intent integration

`tsift ast-grep rewrite --apply` writes straight to the working tree. That is the
right shape for an exploratory codemod and the wrong shape for one you intend to
land: there is no reindex, no impact report, and no temp worktree to fail in.
The `structural_rewrite` semantic edit intent routes the same engine through
[specs/ast-edits.md](ast-edits.md)'s planning path so a pattern-driven codemod
earns the same proof a symbol-resolved one does.

```json
{ "kind": "structural_rewrite", "file": "src/scan.rs",
  "pattern": "$A.unwrap()", "replacement": "$A.expect(\"scan\")" }
```

- `pattern` is the ast-grep pattern; `replacement` is the rewrite template.
  `pattern` is required by `structural_rewrite` and **refused on every other
  intent kind**, so a pattern silently ignored by a symbol-resolved intent is
  not representable.
- The target is a file, not a symbol — this is the only intent kind that selects
  by shape. `symbol` / `target_handle` are not used.
- The executor is resolved from the file exactly as for any other intent, then
  its ast-grep grammar is resolved from that executor's contract `id`. An
  executor whose language has no grammar compiled into the build is a refusal
  naming the structural languages that are, never a skip.
- Input and rewritten buffer are both reparsed with the executor's tree-sitter
  grammar. The rewrite template is raw text, so this reparse is the only thing
  standing between a template typo and a syntactically broken file.
- Both degenerate outcomes fail closed rather than planning an empty edit: a
  pattern that matched nothing did not express what the caller meant, and a
  template that reproduces its own match would report a no-op as completed work.
  This is stricter than the `ast-grep rewrite` reporting contract above, which
  merely flags `unchanged` — a plan that cannot mutate must not be applyable.
- Everything downstream is unchanged: patch proposal, diff preview, `--verify`
  in a detached temp worktree with reindex and `impact`, formatter policy,
  `expected_content_hash` conflict detection, and batch rollback.

## Known limits

### Per-grammar pattern quirks

A pattern is parsed by the target grammar as a standalone fragment. Where a
grammar cannot parse a bare expression, patterns must be spelled differently.
These are upstream grammar properties, not tsift defects, and each is pinned by
a test in `lang.rs` so an ast-grep bump cannot silently invalidate this table.

| Language | Quirk | Workaround |
|---|---|---|
| **C** | `foo($A)` matches nothing — tree-sitter-c reads it as a *declaration* (`foo` a type, `$A` a declarator), not a call | Add the trailing semicolon: `foo($A);` |
| **CSS** | `color: $V` matches nothing | Add the trailing semicolon: `color: $V;` |
| **Dart** | Expression- and statement-level patterns match nothing however they are spelled; the grammar cannot parse a bare expression fragment | Match at declaration granularity: `void main() { print($A); }`. Call-site codemods are **not** available in Dart |
| **Solidity** | Same as Dart: statement-level patterns match nothing | Match whole declarations: `function f() public { $$$B }` |
| **Rust** | Macro arguments are `token_tree`, so a call inside `assert_eq!(...)` is invisible | None; patterns aimed at macro-heavy code under-report |

Everything else added in the structural-only tier — go, cpp, csharp, java,
elixir, haskell, hcl, html, json, lua, nix, php, ruby, scala, swift, yaml —
matches expression-level patterns directly with no special spelling.

### General

- **Macro bodies are opaque.** tree-sitter parses Rust macro arguments as
  `token_tree`, so `AstGrepLang::from_name($X)` matches a real call but not the
  same call inside `assert_eq!(...)`. This is a grammar property, not a tsift
  defect; patterns aimed at macro-heavy code will under-report.
- Matching is non-reentrant: nested occurrences of a pattern inside its own
  match (`Some(Some($A))` against `Some($A)`) yield the outer match only.
- There is no index involvement, so a structural scan is O(tree) and does not
  require `tsift index` to be fresh.

## Verification requirements

- Rewrite coverage must include a growing replacement (proves reverse-apply),
  a multibyte source (proves UTF-8 boundary handling), and a multi-match file
  (the one-shot `AstGrep::replace` edits only the first match — that is the
  regression this guards).
- CLI coverage must assert that preview leaves target files byte-identical and
  that `--apply` under a budget exits non-zero *without writing*.
- `structural_rewrite` coverage must include a multi-match apply (proves the
  codemod is not one-shot), a dry run that reports a diff and writes nothing, a
  no-match refusal, and a `--verify` run that reaches `temp_applied_total > 0`
  with the real tree byte-identical. A test asserting only that the intent kind
  is *accepted* would pass against a dispatch that never reaches ast-grep.
