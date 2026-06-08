# tsift Spec — AST/CST Edits

Part of the [tsift spec](../SPEC.md). See that index for the full command/spec map.

## Purpose

`tsift edit-intents` is the semantic write surface for agents that need to change source without losing the token and handle discipline of `search`, `source-read`, `symbol-read`, `markdown-ast`, and graph traversal. The edit path is intentionally hybrid:

- AST and Markdown AST projections select, disambiguate, and validate targets.
- CST or source-aware parsers preserve trivia where the language support can prove it.
- The mutation boundary is still a minimal textual patch against the current file bytes.

AST nodes are never treated as a pretty-printed replacement for the file. A supported edit must prove that it can map the selected semantic target back to exact byte ranges, emit bounded text edits, and re-validate the resulting source.

## Strategy

Each edit intent resolves through the same handles and spans used by read/navigation commands. A target can come from an indexed symbol row, an AST span handle, a Markdown `mdast-*`/`span-*` handle, a `search` ranked result, a `symbol-read` body/span, or a graph traversal result that points back to a concrete source file and symbol. Resolution must report the source file, canonical symbol or Markdown section identity, current content hash, AST node kind, line range, byte range, and body byte range when applicable.

The planner then chooses a language executor. Executors own language-specific parser support, formatter policy, supported intent kinds, and the mapping from semantic targets to text edits. A dry run is the default behavior: it returns an execution plan and diff preview without mutating the source tree.

When `--apply` is requested, tsift composes per-file non-overlapping text edits, writes staged files beside their targets, validates every staged file, formats only when the language contract requires it, and swaps staged files into place atomically with rollback on failure. Multi-file batches must either complete all planned file swaps or restore the original files before returning an error.

## Edit Target Selection

Semantic edit intents may identify a target with `symbol`/`file`, or with `target_handle` when a previous tsift read/navigation command already selected the node. The handle-selection prototype is read-only during dry run: it scans the current index, recomputes known concrete handle families, and maps the requested handle to one indexed AST span before patch planning begins.

Supported concrete handle families:

- `span-*` from `search` AST artifacts, `source-read` symbol refs, `symbol-read`, Markdown AST span refs, or traversal AST-span nodes
- `ssym-*` from `source-read` symbol refs
- `sread-*` from `symbol-read` target packets
- `gsym-*` from traversal graph symbol nodes

The resulting plan includes `target_selection` with the requested handle, matched handle, handle family, source surface, file/name/kind/language, full span metadata, a bounded source-window command, and a `symbol-read` command. This proof is additive: `target_symbol` and `target_range` still carry the normalized target used by existing edit planning.

Non-concrete search preview handles such as `sfam-*`, `srnk-*`, and lexical file-hit handles are not writable targets by themselves because their stable hashes intentionally omit enough reverse-mapping context to select a unique current AST node. They must fail closed with guidance to pass the nested `ast.span.handle` from the search result instead.

## Minimal Textual Patch Output

Semantic edits emit text patches, not synthetic whole-file rewrites. For every planned file, the report must expose:

- the target file and current content hash
- target symbol or Markdown node metadata, including stable handle/span information when available
- `patch_proposal` with schema version, `ast_cst_minimal_textual_patch` strategy, parser input/output validation status, trivia-preservation policy, per-file hashes, and ordered hunks
- ordered text edit ranges in byte coordinates, with line-range previews for humans
- a bounded unified diff preview in dry-run and apply modes
- whether the diff was truncated, plus an expansion path when the full diff is artifact-backed
- formatter and validator decisions that changed or rejected the patch

Generated patches should touch only the selected ranges plus required structural companions such as imports, module declarations, or same-file call-site updates. If formatting expands the diff outside the expected region, the executor must either justify that expansion in the report or refuse before mutating the real tree.

`patch_proposal` is emitted only after parser validation succeeds for the current input and proposed output. Unsupported parser states, parse-error WIP files, unresolved parser languages, or proposal output that cannot reparse produce `status=unsupported`, `apply_supported=false`, a refusal message, and no patch proposal.

## Refusal Modes

The edit path fails closed before mutation when any invariant needed for a minimal, verifiable patch is missing. Required refusal cases include:

- unsupported language, parser, file extension, or intent kind
- a target handle that cannot be resolved to the current file contents
- missing or mismatched `expected_content_hash` when the caller supplied one
- ambiguous symbols, Markdown headings, list items, code fences, or graph-derived targets
- parse errors that overlap the target or prevent trustworthy range extraction
- generated files, generated sections, macro expansions, or embedded-language islands whose writable source range is not concrete
- overlapping edits within a batch
- cross-file call-site rewrites that the executor cannot prove complete
- formatter, syntax validation, reindexing, impact, verification-command, or temp-worktree failures
- any planned patch whose output diff includes unexplained edits outside the executor's declared range policy

A refusal is a first-class result. It must include the intent kind, target file if known, status, `apply_supported=false` when applicable, and a concise message that names the failed invariant.

## Diff Visibility

No successful plan or apply may hide the mutation. Dry-run reports include a diff preview even when the caller asks for compact or envelope output. If the diff exceeds the active response budget, tsift returns a truncated preview with counts and an explicit expansion command or artifact reference.

Apply reports include the same diff contract plus `applied=true` only after the real source tree was changed. If `--verify` succeeds in a temp worktree but the real apply later fails, the report must keep `applied=false` for the failed plan and describe the rollback state.

## Verification Requirements

Dry-run planning verifies target uniqueness, range extraction, parser support, batch overlap, and patch construction. It must reparse the staged buffer for supported languages before reporting a plan as apply-capable.

`--verify` is stronger than dry run and weaker than real apply. It must use a detached temporary git worktree, apply the supported edits there, reindex before and after the temp mutation, run bounded `source-read` windows for changed targets, run `impact`, and run the optional `--verify-command` when supplied. Verification failure must leave the real tree untouched.

`--apply` may mutate the real tree only after the current files still match the planned content hashes or the caller has supplied an explicit fresh plan. The executor must reparse/validate after composing edits and after formatting. If the language contract has a formatter, formatter failure is a refusal unless the contract explicitly marks formatting optional for that language and intent.

## Promotion Order

New AST/CST edit operations are promoted narrowly. `insert_import` and `replace_function_body` are the baseline operations because their target ranges and expected diffs are easy to inspect.

For Rust, `replace_function_body` must select a parsed `function_item` body. When the intent includes a concrete target handle or indexed span, the executor must match that exact span before replacing bytes so duplicate function names do not silently edit the first textual match. Without a concrete span, duplicate same-file function names are ambiguous and must fail closed.

For Rust, `insert_import` must parse the current source and anchor insertion after the source-file prelude that can safely precede imports: shebangs, crate-level inner doc comments, inner attributes, `use` declarations, and `extern crate` declarations. The emitted mutation is still a minimal textual insertion and must reparse before planning or applying.

Broader rename, move, call-site, and signature operations require additional graph/index proof and tests that cover comments, formatting preservation, unsupported parser states, macro or generated regions, syntax-error work-in-progress files, and verification failures.
