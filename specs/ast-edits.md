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

Semantic edit intents may identify a target with `symbol`/`file`, or with `target_handle` when a previous tsift read/navigation command already selected the node. The one exception is `structural_rewrite`, which selects by *shape*: it takes `file` plus an ast-grep `pattern` and has no resolved symbol at all. See the Structural Intents section below. The handle-selection prototype is read-only during dry run: it scans the current index, recomputes known concrete handle families, and maps the requested handle to one indexed AST span before patch planning begins.

Supported concrete handle families:

- `span-*` from `search` AST artifacts, `source-read` symbol refs, `symbol-read`, Markdown AST span refs, or traversal AST-span nodes
- `ssym-*` from `source-read` symbol refs
- `sread-*` from `symbol-read` target packets
- `gsym-*` from traversal graph symbol nodes

The resulting plan includes `target_selection` with the requested handle, matched handle, handle family, source surface, file/name/kind/language, full span metadata, a bounded source-window command, and a `symbol-read` command. This proof is additive: `target_symbol` and `target_range` still carry the normalized target used by existing edit planning.

Non-concrete search preview handles such as `sfam-*`, `srnk-*`, and lexical file-hit handles are not writable targets by themselves because their stable hashes intentionally omit enough reverse-mapping context to select a unique current AST node. They must fail closed with guidance to pass the nested `ast.span.handle` from the search result instead.

## Range-Selected Intents

`extract_function` is the only intent that selects a **range** rather than a named target. A run of sibling statements is not one AST node, so `target_handle` cannot express it; the intent takes `file` plus a one-based inclusive `start_line`/`end_line`, and those two fields are rejected for every other kind so a stray range cannot silently widen an edit that resolved its target by name. Like `structural_rewrite`, it is dispatched ahead of the per-family executor split because there is no symbol to resolve.

The output is not a rewrite of what was selected. It is two edits that must agree: a new function whose signature is *derived* from the selection, and a call whose arguments come from the same derivation. Both are computed from one analysis of the original bytes, and the new function is spliced after the call so neither edit moves the other's offsets.

The derivation, for a run `R` inside enclosing function `F`:

- **Parameters** are names read in `R` before `R` assigns them, that are bound in `F` outside `R` (including `F`'s own parameters). A name `R` assigns before reading is a local of the new function: passing the outer value in would feed the body a value it immediately overwrites.
- **Returns** are names `R` assigns that are read in `F` after `R`. None emits a bare call, one emits `name = call(...)`, several emit a destructuring, which is the only spelling that keeps the call site one statement.
- A name bound at module scope is neither. Threading it through a parameter would compile and quietly change what the new function closes over, which is why module scope is classified explicitly rather than inferred from "not bound in `F`".

Everything else refuses, and the refusal list is load-bearing: a rename that misses an occurrence breaks the build loudly, but an extraction with a wrong parameter list can compile and silently change behaviour. Refusals cover a range that is not a contiguous run of siblings in one block, a range outside any function, an enclosing function that sits in an *expression* rather than a statement (an arrow function, a function expression, a class expression — there is no statement slot beside it), control flow that escapes the range (`return`, `break`, `continue`, `yield`), a range that names `this` or `super`, a range assigning a name `F` declared `global`/`nonlocal`, a new name that already binds in `F`, at module scope, or beside the insertion point, several returned names in a language with no destructuring call site, a return set that mixes newly declared with already declared names, and a parameter whose type cannot be spelled.

### Where the new function goes

Usually beside the enclosing function. Inside a **method** that is wrong: a `def` placed beside a method is another method, and the bare call left behind does not resolve to it. So the insertion point climbs to the nearest position where the call can still see it, which differs by language for a reason worth stating:

| | insertion point | receiver |
|---|---|---|
| Python | past the class, at module scope | `self` is an ordinary name and threads through the signature, first in the parameter list |
| JS / JSX / TS / TSX | beside the class declaration | `this` is not a name a signature can carry, so a range that uses it **refuses** |
| GDScript | beside the method, inside the class | resolves bare against the script's own members, so staying is what keeps the call working |

Climbing past a class body never costs the extracted body anything, because a method could not read a class-body name unqualified in the first place. Climbing past a *function* would — a nested function closes over its enclosing function's locals — so the climb stops at the first non-class statement position. Name collisions are checked against the block the new function actually lands in as well as against module scope, which is how a GDScript sibling method conflict is caught at all: a file-root scan never sees the members of a class.

### The untyped family

Registration is per language, not per family, and the family that carries the kind is the set of languages whose signature is derivable without type information: **Python, GDScript, JavaScript, JSX, TypeScript, and TSX**. Rust is deliberately absent — choosing `T`, `&T`, or `&mut T` needs types `tsift-graph` does not have, and a guessed signature parses without building. Advertising the kind for an executor with no emitter would be the same defect as advertising `structural_rewrite` for a language with no compiled grammar.

One analysis serves all six; what varies is a node-kind vocabulary and an emitter:

| | block | new function | call site | several returns |
|---|---|---|---|---|
| Python | indentation | `def name(a, b):` | `x, y = name(a, b)` | tuple destructuring |
| GDScript | indentation | `func name(a, b):` | `var x = name(a, b)` | **refused** — no destructuring assignment |
| JS / JSX / TS / TSX | braces | `function name(a, b) {` | `let [x, y] = name(a, b);` | array destructuring |

Two consequences follow from languages that *declare*:

- The call site declares (`let`/`var`) only when the range carried the declaration away with it, and assigns when the name still exists outside. A return set that mixes the two refuses rather than splitting the call into statements the caller did not ask for.
- The mirror of that rule applies inside the new function: a name the range only *assigns*, whose declaration stayed behind, is declared in a prologue — otherwise the emitted body writes a name that is not in scope, which TypeScript rejects and plain JavaScript turns into a global. A name that is not a local of the enclosing function refuses instead (`AssignsUndeclaredName`), because declaring it would shadow an outer binding and not declaring it would write to a scope the caller did not choose. Python is exempt by construction: there, assignment declares.
- One level of indentation is measured from the enclosing function's own body, not assumed, so a tab-indented `.gd` file and a two-space `.py` file each get a new function indented the way they already are.

TypeScript is in the family only because it can **copy** an annotation the file already has, from the parameter or the declaration that binds the name. Where it cannot, it refuses — `unknown` and an implicitly `any` parameter both type-check something other than what the code does, which is the failure this intent exists to avoid.

Every registered executor carries one extraction conformance fixture asserting its emitted function and its call site individually, and the planner reparses its own spliced output with the executor's grammar before returning it.

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

## Structural Intents

`structural_rewrite` is the pattern-driven intent kind. It exists because the
two selection mechanisms above both start from a name: an index row or a handle
that already resolved to one symbol. A refactor usually starts from a *shape*
instead — "every `.unwrap()` on this expression", "every call passing this
argument" — which no symbol resolution can express. The matching engine is
[specs/structural-patterns.md](structural-patterns.md); this section owns only
how that engine enters the edit path.

The intent carries `file`, `pattern` (an ast-grep pattern), and `replacement`
(the rewrite template). `pattern` is required by `structural_rewrite` and
refused on every other kind, so it can never be silently ignored by an intent
that does not read it. The kind requires no `symbol` and no `target_handle`.

Language resolution goes through the file's registered executor, then through
that executor's contract `id` to an ast-grep grammar. An executor language with
no grammar compiled into the build is a refusal naming the structural languages
that are — never a skip, because the caller named this file explicitly.

Planning is otherwise identical to any other intent. Both the input and the
rewritten buffer are reparsed with the executor grammar; because the rewrite
template is raw text, that output reparse is the only guard against a template
typo producing unparseable source. Two outcomes that the standalone
`ast-grep rewrite` surface merely reports are refusals here, since a plan that
cannot mutate must not be applyable: a pattern matching nothing, and a template
that reproduces every match unchanged.

Everything downstream — patch proposal, bounded diff preview, formatter policy,
`expected_content_hash` conflict detection, `--verify` in a detached temp
worktree with reindex plus `impact`, `--apply`, and batch rollback — is shared
with the symbol-resolved kinds and is not re-specified for this kind.

### Structural-only executors

Because `structural_rewrite` needs only a grammar to match with and a grammar to
reparse the result, a language can be a *structural-only* executor: registered,
with `structural_rewrite` as its whole recognized-intent set, and no
language-specific rewriting behind the symbol-resolved kinds. Rewriting requires
a **parser**; indexing is what requires tag queries and symbol extraction. The
two are independent capabilities, so a language that is not indexed, searchable,
or graphable is still a semantic-edit executor for this one kind.

The reparse grammar therefore resolves in two steps, and an executor with
neither is a registration bug refused by name rather than parsed with some other
language's rules:

| Executor | Reparse grammar |
|---|---|
| has a `tsift-graph` binding | that binding's grammar — Markdown in particular parses through `tsift-md-ast`, not ast-grep's `tree-sitter-md` |
| structural-only | the ast-grep grammar its pattern matched against |

For every executor that has both, those two grammars are currently the same
object — even Kotlin, where ast-grep's `tree-sitter-kotlin` feature resolves to
the same `-ng` grammar `tsift-graph` uses. That agreement is load-bearing and
invisible, so it is asserted rather than assumed: a rewrite is *matched* with
the ast-grep grammar, and validating its output against a different one would
report a clean parse for source the matcher could never have produced, or refuse
source it would have. A grammar bump that splits the two must fail the suite and
force the choice explicitly.

An executor that does not recognize a kind must be **refused before the family
split**, not carried into it. The split routes anything that is neither
markdown, script, nor indexed-generic to the Rust implementations, so an
unrecognized kind would otherwise be rewritten by Rust identifier rules and
reported as applied through that language's executor — plausible output produced
by the wrong language's logic, which is worse than a refusal. The refusal names
the executor and its supported kinds.

### Indexed executors

`rename_symbol` is not per-language work. Identifier occurrences come out of the
grammar, and the rename's cross-file extent comes out of the call graph; both
are things every indexed language already has. An *indexed executor* is
therefore a language with a `tsift-graph` binding and no hand-written per-kind
rewriting, whose recognized set is `rename_symbol` plus `structural_rewrite`
where a grammar for the latter exists. Registration is a per-language
identifier-node-kind set and a fixture row, not another copy of a rewriting
implementation. The kinds that stay unrecognized for this tier —
`replace_function_body`, `insert_import`, `add_method` — are the ones that
genuinely do need language-specific rewriting.

The two tiers are distinguished by what they lack, and the distinction is not
cosmetic:

| Tier | `tsift-graph` binding | ast-grep grammar | Recognized kinds |
|---|---|---|---|
| indexed | yes | yes | `rename_symbol`, `structural_rewrite` |
| indexed, no grammar in this build | yes | no | `rename_symbol` |
| structural-only | no | yes | `structural_rewrite` |

An indexed executor with no ast-grep grammar must **not** advertise
`structural_rewrite`: doing so plans an edit that can only fail at match time.
Dropping it from the recognized set turns that into a refusal at registration,
which is the difference between a plan that is declined and a plan that is
accepted and then breaks.

Rename scope is keyed per language, not per tier. `callers_of` and
`symbol_info` match by *name*, so a rename must be told which languages could
actually be referring to the same symbol: the JS-like executors are one family
because they do call each other, and every language in the indexed and
structural tiers is its own family, because a Bash `deploy` and a Zig `deploy`
sharing a name is a coincidence. Keying scope on the tier instead would
reintroduce the defect where a Python `beta` blocked renaming a JavaScript one.

Bash is the one indexed language where the node kind alone does not identify a
name. A bare `word` is the function name, the command name, **and** every
unquoted argument, so `echo deploy` would otherwise have a rename rewrite an
argument that is data — the same class of bug as renaming inside a string
literal. `word` occurrences are therefore restricted to the declaration and
command-name positions; `variable_name` needs no such guard, because it is only
ever a variable.

### Which symbol a rename is renaming

A node-kind filter is coarser than the language. Several grammars spell two
unrelated declarations identically: a Rust struct field and the method in
`x.method()` are both `field_identifier`; a GDScript `func` and a local `var`
are both `name`. Kind alone therefore has a rename of one rewrite the other.

The resolved symbol's indexed kind is the missing input, and it is already
available — the planner had to resolve the symbol before it could plan
anything. It maps onto the small set a grammar can actually check (callable,
signal, type, value), and the occurrence walk drops positions that cannot be
that thing: a function rename skips `struct S { count: usize }`, `S { count: 1 }`,
and `x.count` while keeping `x.count()`; a GDScript function rename skips
`var count`, and a variable rename skips `func count`.

Three rules bound this, and each is a refusal to guess rather than a
convenience:

- **Only unambiguous positions are dropped.** A bare GDScript `count`
  reference, or a Rust `x.count()` that might be a trait method on an unrelated
  type, stays. Under-renaming leaves a call site pointing at a name that no
  longer exists, which is a worse failure than the over-renaming it avoids —
  the caller sees a broken build either way, but only under-renaming reports
  success.
- **A dropped declaration that *shadows* the target is a refusal, not a
  narrowing.** Keeping a GDScript local `var count` while still rewriting
  `return count` would leave the declaration on the old name and its read on
  the new one, which is incoherent in a way neither renaming both nor renaming
  neither is. Where a rejected declaration shadows the target and an
  unattributable reference to it survives, the rename refuses and names the
  shadow's line — the same principle as refusing a cross-file reference the
  call graph cannot attribute. A callee is never ambiguous: `count()` is the
  function whatever else is in scope, so a shadow that is only ever called does
  not trigger the refusal. Rust needs none of this, because a field and a
  function are reached through different syntax and no reference between them
  is ambiguous.
- **An unresolved or unrecognized symbol kind narrows nothing.** A new capture
  name added to `Lang::symbol_query` without a decision about what it is falls
  through to the permissive answer; it must not silently start dropping
  occurrences.
- **Macro bodies are exempt by construction.** tree-sitter parses Rust macro
  arguments as an opaque `token_tree`, so `m.count` inside `format!` is a bare
  `identifier` with no `field_expression` around it and the position rule has
  nothing to read. It is renamed. That is the same opacity that lets the walk
  reach real call sites inside `assert_eq!`, so it is a known and tested
  limitation rather than an oversight.

The JS-like family needs one more move, because a shorthand is one token doing
two jobs. `property_identifier` covers an object-literal key, a class method,
and a member access, and none of those is the module-level binding a rename
resolves to — the symbol query indexes `function_declaration`,
`class_declaration`, and arrow-valued `variable_declarator`, nothing else — so
a resolved rename drops all of them. But `{ beta }` is *both* the property name
and a read of the binding: overwriting the span renames the property as a side
effect, and skipping it leaves a read of a name that no longer exists. Neither
is acceptable, so the occurrence expands to `beta: gamma`, which is exactly
what the shorthand desugars to and keeps both correct.

The destructuring form `const { beta } = mod` is deliberately *not* expanded.
There the token reads a property off `mod` and declares a local of the same
name, so the right rewrite depends on whether `mod` is the module whose export
was renamed. That is the common case, and plain span renaming already gets it
right; expanding would break it.

Python and Kotlin need the same positional distinction despite using only a
flat `identifier` kind. Python's attribute name in `obj.count` is an
`identifier` under `attribute`; Kotlin's member is a non-receiver `identifier`
under `navigation_expression`. Neither position is the module-level binding a
resolved rename selected, so a read such as `obj.count` stays untouched. Both
languages also index methods as callables, however, so the member remains a
valid callable target when that whole attribute/navigation expression is the
callee of a call: `obj.count()` is renamed along with a same-named method
declaration. The receiver identifier is still an ordinary binding reference and
is never dropped by the member-position rule.

Kotlin keeps a member whose receiver names a `class`, `object`, or `interface`
declared in the same file: `Panel.widgetCount` reaches a companion member and
`Registry.widgetCount` an `object` member, both of which the index holds as
declarations. It also recognizes names bound by explicit imports, including
`as` aliases, so qualified access to a type or object declared in another file
does not fall through to the callee-only rule. Wildcard imports remain
unresolved because the parse tree cannot prove which declaration they bind.

Python has one more exception, for the same reason Zig below is narrowed by its
receiver: `mod.name` *is* the module-level binding when `mod` is a module this
file imported, and `import mod` is half of how Python spells a cross-module
reference. A Python attribute is therefore kept, called or not, when its
receiver chain roots in a name bound by an `import` statement — the alias when
there is one, otherwise the first segment of the dotted path, since `import
pkg.mod` binds `pkg`. `from mod import name` binds the name directly and never
produces an attribute position, so it needs nothing. A local that shadows an
imported module resolves to "module" and keeps the occurrence, which is the
over-renaming direction this walk prefers.

Zig has the same flat `identifier` kind and the same member node — the `member`
field of `field_expression` — but the callee-only rule is **wrong** for it, and
Zig is therefore narrowed by the receiver instead. Zig has no
import-into-namespace form: `@import("m.zig").name` and `Type.name` are the only
ways to name a declaration in another file, and both are member positions. A
callee-only rule would drop every cross-file reference to a renamed `const`,
type, or uncalled function while renaming the declaration, which reports success
and breaks the build. So a Zig member is kept whenever its receiver chain roots
in a namespace — an `@import` binding, or a binding whose initializer is a
`struct`/`enum`/`union`/`opaque` declaration, since a Zig container type is also
the namespace holding its declarations — and is dropped only when the receiver
is an ordinary value, where the member is a struct field. The callee exception
still applies to that value case, because Zig indexes methods as
`function_declaration`. A `container_field` name is dropped for every resolved
target: no capture in the Zig symbol query produces one, so a field declaration
is never the symbol a rename selected.

### Conformance fixtures

Two tables, each exhaustive in both directions against the recognized-intent
set: a structural fixture for every executor that recognizes
`structural_rewrite`, and a rename fixture for every executor that recognizes
`rename_symbol`. An executor without its fixture is registered but never
exercised; a fixture without an executor has outlived what it claimed to cover.
A `Lang` variant brought into the renamable tier with no rename row fails the
suite, the same way `packages/tsift-graph/tests/conformance.rs` treats symbol
extraction.

Each fixture drives the real planner path, and the suite compares the sum of
replacements *the planner returned* against the sum the table declares, so a
runner counting its own iterations cannot report green over a table that
rewrote nothing. Rename rows additionally declare the positions that must change
**and** the positions that must survive byte for byte — comments, string
literals, and data that merely shares the name — asserted individually, because
a row that only counted replacements would pass while renaming the wrong ones.
Grammar quirks (C and CSS needing a statement terminator, Dart and Solidity
matching only whole declarations, HCL matching only as an attribute, JSON
needing both sides metavariable-shaped) are row data, so a grammar upgrade that
lifts a limit fails the suite instead of leaving a stale note behind.

## Promotion Order

New AST/CST edit operations are promoted narrowly. `insert_import` and `replace_function_body` are the baseline operations because their target ranges and expected diffs are easy to inspect.

For Rust, `replace_function_body` must select a parsed `function_item` body. When the intent includes a concrete target handle or indexed span, the executor must match that exact span before replacing bytes so duplicate function names do not silently edit the first textual match. Without a concrete span, duplicate same-file function names are ambiguous and must fail closed.

For Rust, `insert_import` must parse the current source and anchor insertion after the source-file prelude that can safely precede imports: shebangs, crate-level inner doc comments, inner attributes, `use` declarations, and `extern crate` declarations. The emitted mutation is still a minimal textual insertion and must reparse before planning or applying.

Broader rename, move, call-site, and signature operations require additional graph/index proof and tests that cover comments, formatting preservation, unsupported parser states, macro or generated regions, syntax-error work-in-progress files, and verification failures.

A language may therefore be registered as a structural-only executor as soon as it has an ast-grep grammar; graph symbol extraction is not a prerequisite, because reparsing is a parser-level need. The symbol-resolved kinds stay unrecognized for it until their per-language rewriting exists — and for a language with no index they stay unreachable anyway, since nothing resolves a symbol to target. That unreachability is where the refusal for this tier actually comes from: the index layer answers `no indexed symbol matched` before any executor is consulted, so the executor-level refusal is a defence in depth for it rather than the path a caller hits. Coverage for such an executor must include an applied structural codemod *and* a symbol-resolved kind refused without writing — a test that only checks the codemod would pass against a build that silently ran another family's implementation for everything else.

`structural_rewrite` is promoted across every registered executor at once rather than language by language, because its selection and mutation logic are language-independent: the grammar is a parameter, not a code path. What is language-specific — parser validation and formatter policy — is already owned by the executor contract. Coverage must therefore include at least one non-Rust executor and a drift guard asserting that every executor advertising structural support resolves an ast-grep grammar, so a newly added executor language cannot advertise structural support it has no parser for.

`rename_symbol` is promoted the same way and for the same reason: once occurrences come out of the grammar and extent comes out of the call graph, the language is a parameter. Bringing an indexed language into the renamable tier is a per-language identifier-node-kind set plus a rename fixture row, and both are enforced — a `Lang` variant with no identifier kinds, or a renamable executor with no fixture, fails the suite. Coverage must reach the end-to-end path, not the planner alone, and must include an executor that recognizes one kind while refusing another: an executor that refuses *everything* cannot distinguish a working guard from a broken family split.
