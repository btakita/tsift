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
- The tsift `lang-*` features fan out to `tsift-astgrep/lang-*`.
- **`lang-zig` forwards nothing**: `ast-grep-language` ships no Zig grammar, so
  Zig is indexable by tsift but not structurally matchable.

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

## Known limits

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
