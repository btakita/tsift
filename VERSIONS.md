# Versions

tsift is private software, but its command surface is still versioned explicitly.

Canonical binary version source: `Cargo.toml` `package.version`. The CLI exposes the same value via `tsift --version`.

Use `BREAKING CHANGE:` prefix in version entries to flag incompatible changes.

## Unreleased

- **`extract_function`, the first range-selected edit intent.** Every
  other semantic edit selects a named thing — a symbol row, a heading, an
  ast-grep pattern — and rewrites at or around it. This one selects a run of
  sibling statements, which has no name, no symbol row, and no single AST node,
  so it takes a one-based `start_line`/`end_line` instead of a `target_handle`.
  Those fields are refused for every other kind, so a stray range cannot widen
  an edit that resolved its target by name.

  What comes out is two edits that have to agree: a new function whose signature
  is *derived* from the selection, and a call whose arguments come from the same
  derivation. Parameters are names read before the range assigns them that are
  bound in the enclosing function; returns are names the range assigns that are
  read after it; a module-scope name is neither, and stays a free reference.
  That last rule is the one worth stating out loud — threading a module global
  through a parameter compiles and quietly changes what the new function closes
  over, which is the failure mode this intent is organized against.

  Everything else refuses by name: a selection that is not a contiguous run of
  siblings in one block, a range outside any function, an enclosing function
  that sits in an *expression* rather than a statement, control flow that
  escapes the range, a range that names `this` or `super`, a range assigning a
  `global` or `nonlocal` name, and a new name that already binds in scope.
  `return` and `yield` escape unconditionally; `break` and `continue` only when
  the loop — or, in JavaScript, the `switch` — they bind to stays behind. A
  range holding a whole loop takes that loop's `break` with it and nothing
  changes, and refusing there would decline the most ordinary extraction there
  is. A labelled branch is checked against the labels the range itself carries. A rename that misses an occurrence breaks
  the build loudly; an extraction with a wrong parameter list does not, so
  under-refusing here is the expensive direction.

  **Where the new function goes** is the other half of that. A `def` placed
  beside a method *is* another method, and the bare call left behind does not
  resolve to it — code that parses, formats, and raises at run time. So the
  insertion point climbs out of a class: past it to module scope in Python,
  where `self` is an ordinary name and threads through the signature first in
  the list; beside the class declaration in JavaScript and TypeScript; and for
  GDScript deliberately nowhere, because its methods call each other bare and a
  sibling `func` is exactly what the call needs. A JS/TS range that names `this`
  refuses instead — unlike `self`, it is not a name a derived signature can
  carry. The climb stops at the first non-class statement position, so a nested
  function's extraction stays inside its enclosing function and keeps its
  closure, and a function that sits in an expression still refuses because there
  is no statement slot beside it. Collisions are checked against the block the
  new function lands in as well as against module scope, which is the only way a
  GDScript sibling-method conflict is seen at all.

  Registered for the untyped family: Python, GDScript, JavaScript, JSX,
  TypeScript, and TSX — the languages whose signature is derivable without type
  information. One analysis serves all six; what varies is a node-kind
  vocabulary and an emitter for the `def`/`func`/`function` spelling and the
  indentation or brace block. Two things follow from the languages that
  *declare*: the call site says `let`/`var` only when the range carried the
  declaration away with it (and refuses a return set that mixes declared with
  already-declared names, which one statement cannot receive), and the new
  function declares, in a prologue, every name the range only *assigns* —
  without that its body writes a name that is not in scope, which TypeScript
  rejects and plain JavaScript turns into a global. A name that is not a local
  of the enclosing function refuses instead. One level of indentation is
  measured from the enclosing function's own body rather than assumed, so a
  tab-indented `.gd` file stays tab-indented. GDScript refuses
  more than one returned name outright: it has no destructuring assignment, and
  an array plus two index reads would be three statements where the caller
  wrote one.

  TypeScript is in the family only because it can *copy* an annotation the file
  already has. Where it cannot, it refuses — `unknown` and an implicitly `any`
  parameter both type-check something other than what the code does.

  Rust is still out. Choosing `T`, `&T`, or `&mut T` needs types tsift does not
  have, so it is a separate decision (caller-supplied signature, or a closure
  that infers) rather than a registration entry. Plan:
  `tasks/software/plan-tsift-extract-function.md`.

## 0.1.79

- **A rename now knows which kind of symbol it is renaming.** Renaming a Rust
  function also rewrote an identically-named struct field; renaming a GDScript
  `func` also rewrote an identically-named local `var`. Both grammars spell the
  two with the same node kind — `field_identifier` covers a struct field *and*
  the method in `x.method()`, `name` covers every GDScript declaration from
  `func` to `var` — so a kind filter alone cannot separate them.

  The resolved symbol's indexed kind was the missing input, and it was already
  in hand: the planner resolves the symbol before it plans anything. It now
  reaches the occurrence walk, which drops positions that cannot be that thing.
  A Rust function rename skips `struct S { count: usize }`, `S { count: 1 }`,
  and `x.count` while keeping `x.count()`. A GDScript function rename skips
  `var count` and parameter bindings; a variable rename skips `func count`.

  Three limits, each a refusal to guess:

  Only positions the grammar makes unambiguous are dropped. A bare GDScript
  `count` reference stays, because under-renaming leaves a caller pointing at a
  name that no longer exists — the same failure the cross-file work existed to
  fix, and the one that reports success while breaking the build.

  Where a dropped declaration *shadows* the target and an unattributable
  reference to it survives, the rename **refuses and names the shadow's line**
  rather than narrowing. Keeping `var count` while rewriting `return count`
  would leave the declaration on the old name and its read on the new one,
  which is incoherent in a way that neither renaming both nor renaming neither
  is. A callee is never ambiguous, so a shadow that is only ever called does
  not trigger it. This was found by reading the applied output of a live
  rename, not by a failing test.

  An unresolved or unrecognized symbol kind narrows nothing. A capture name
  added to `Lang::symbol_query` without a decision about what it is falls
  through to the permissive answer rather than silently dropping occurrences.

  Rust macro bodies are exempt by construction: tree-sitter parses macro
  arguments as an opaque `token_tree`, so `m.count` inside `format!` is a bare
  `identifier` with no `field_expression` to read. It is still renamed. That is
  the same opacity that lets the walk reach real call sites inside
  `assert_eq!`, so it is now a tested limitation rather than folklore.

  The same narrowing now covers TypeScript, TSX, JavaScript, and JSX, where
  `property_identifier` spans an object-literal key, a class method, and a
  member access — none of which is the module-level binding a rename resolves
  to. Renaming a top-level `beta` no longer rewrites `{ beta: 1 }`,
  `class Panel { beta() {} }`, or `keyed.beta`.

  The object-literal shorthand `{ beta }` is the case with no correct span
  rewrite: it is both the property name and a read of the binding, so
  overwriting it renames the property as a side effect and skipping it leaves a
  dangling read. It now **expands to `{ beta: gamma }`**, which is what the
  shorthand desugars to and keeps both correct. The destructuring form
  `const { beta } = mod` is deliberately left as a span rename — there the
  token reads a property off `mod`, and when `mod` is the module whose export
  was renamed, which is the common case, renaming in place is already right.

  Python and Kotlin now apply the same positional narrowing to their flat
  `identifier` node sets. A resolved callable rename leaves the member read in
  `panel.widget_count` / `panel.widgetCount` untouched, while still renaming
  the callee in `panel.widget_count()` / `panel.widgetCount()`. The call stays
  a target because both languages index methods as callables; the receiver
  stays a normal binding reference. Unit tests and the cross-language
  conformance table pin declarations, direct calls, method calls, and
  untouched member reads in both grammars.

  Kotlin keeps a member whose receiver names a `class`, `object`, or
  `interface` declared in the same file, so a companion or `object` member
  survives qualified access. A receiver declared in another file still falls to
  the callee rule; Kotlin's usual cross-file reference is an `import` that binds
  a bare identifier, which was never at risk.

  Python keeps `mod.name` when `mod` is a module the file imported. The
  attribute rule shipped without that exception and dropped it, which renamed
  the definition and left every `import mod` reader pointing at a name that no
  longer exists — a silent under-rename on a cross-file rename path. The alias
  is used when there is one, and `import pkg.mod` binds `pkg`.

  Zig is narrowed by the *receiver*, not by the callee position, because the
  callee-only rule would be a silent under-rename there. Zig has no
  import-into-namespace form, so `@import("m.zig").name` and `Type.name` are
  the only ways to reach another file's declaration and both are member
  positions — dropping them would rename a `const` or an uncalled function and
  leave every cross-file reference behind. A Zig member is now kept when its
  receiver roots in a namespace (an `@import` binding, or a binding whose
  initializer is a container type, which in Zig is also a namespace) and
  dropped when the receiver is an ordinary value, where the member is a struct
  field. A `container_field` declaration is dropped for every resolved target:
  the Zig symbol query never produces one.

- **`rename_symbol` is open to every indexed language.** Kotlin, Bash, Zig, and
  GDScript can now be renamed. They could not before for no reason other than
  the implementation: each family hand-rolled its own substring scan, so a
  language without one was refused even though it was fully indexed.

  Once occurrences come out of the grammar and extent comes out of the call
  graph, the language is a parameter. Registration is a per-language
  identifier-node-kind set plus a conformance fixture row — no new rewriting
  code — and cross-file renames work for these languages with nothing
  language-specific behind them.

  Two things this exposed, neither of them cosmetic:

  **Bash needed a position rule, not just a kind rule.** A bare `word` is the
  function name, the command name, *and* every unquoted argument, so
  `echo deploy` would have had a rename rewrite an argument that is data — the
  same class of bug as renaming inside a string literal, arriving through a
  different door. `word` occurrences are restricted to the declaration and
  command-name positions.

  **Rename scope is keyed per language, not per tier.** Name-matched call edges
  meant a single "structural" family would have had a Zig `deploy` block a
  rename of a Bash `deploy` — the defect that once had a Python `beta` block a
  JavaScript one, reintroduced through the grouping. Each language in the
  indexed and structural tiers is its own rename family; only the JS-like
  executors share one, because they genuinely call each other.

  Zig and GDScript recognize `rename_symbol` and **not** `structural_rewrite`:
  this build compiles no ast-grep grammar for them, and advertising a kind that
  can only fail at match time is worse than refusing it at registration. That
  makes them the first executors that recognize one kind and refuse another, so
  the executor-level refusal guard is now exercised end to end rather than in
  unit tests alone.

  Conformance is two exhaustive tables rather than one: a structural fixture per
  executor that recognizes `structural_rewrite`, and a rename fixture per
  executor that recognizes `rename_symbol`. A `Lang` brought into the renamable
  tier with no rename row fails the suite. Rename rows declare the positions
  that must change **and** the positions that must survive byte for byte, each
  asserted individually — a row that only counted replacements would pass while
  renaming a comment or a string literal.

  Correction to the plan for this phase: it expected the *structural-only*
  `rename_symbol` refusal to become reachable end to end here. It does not.
  Making every indexed language renamable leaves the structural-only tier
  defined by having no `tsift-graph` binding at all, so the index layer answers
  `no indexed symbol matched` before any executor is consulted. That is now
  pinned by a test, so a later change that moves the refusal is noticed rather
  than read as a regression.

- **`rename_symbol` renames through the grammar, and across files.** Two
  correctness defects, both reproduced before the fix and covered by tests
  after it.

  It was a `match_indices` scan with an identifier-boundary guard, duplicated
  per language family. A boundary guard cannot tell an identifier from the same
  characters inside a string literal or a comment, so the rename rewrote both —
  and rewriting a string literal changes what a program does, not what it is
  called. `tsift-graph::rename` now collects occurrences from the parse tree,
  restricted to each grammar's identifier node kinds. Comments and string
  bodies are different node kinds, so they drop out by construction; there is
  no comment or string special case, and a new quoting or comment form cannot
  reintroduce the bug. (tree-sitter parses Rust macro arguments as an opaque
  `token_tree`, so this could have *lost* call sites inside `assert_eq!` that
  the old scan found. Identifiers in there are still named `identifier` nodes,
  so there is no regression, and that case has its own test.)

  It also edited one file and reported `conflicts=0` while leaving every caller
  in every other file referring to a name that no longer exists — a tree that
  does not compile, reported as a success. The rename now resolves its extent
  from the indexed call graph and rewrites every referencing file in the same
  all-or-nothing buffer set.

  Two things make that safe, and both came out of a red suite rather than
  design. Call edges and definition lookups are matched **by name**, and a name
  is not unique: scoping is per executor family, so a Python `beta` neither
  blocks nor is rewritten by a JavaScript rename. And a file that defines its
  own function of the same name looks like a caller of ours — its own
  definition shadows any import, so it is calling itself and is excluded.

  Where the graph genuinely cannot attribute a reference — a cross-file caller
  plus a second same-family definition of the name — the intent **refuses and
  names both**, rather than renaming one of them and hoping. An unrelated
  module that defines the same name but is never called across files is not
  ambiguous and does not trigger the refusal.

  Reports gain `edited_range` (the lines the edit actually rewrote) and
  `rename_caller_files`; the human printer's `range:` line is now
  `target range:`, since `target_range` is the symbol's *declaration* span and
  reading it as the extent of the change was the invitation.

- **GDScript joins the indexed tier.** `.gd` files are now walked, indexed,
  searched, and graphed like any other indexed language, behind the default-on
  `lang-gdscript` feature backed by `tree-sitter-gdscript` 6.1.0.

  Symbol extraction covers the constructs a Godot script is navigated by:
  `func` definitions, inner `class` definitions, the `class_name` statement
  (the name every *other* script refers to the file by, so it has to be a
  symbol even though it is a statement rather than a block), `enum`, `signal`,
  `const`, and `var` — including the `@export` and `@onready` variable forms,
  which the grammar gives distinct node kinds.

  Call edges resolve for all three call shapes: a bare `foo()` (`call`), the
  `node.foo()` member call (`attribute_call`, nested under `attribute`), and
  the Godot-1.x `.foo()` super call (`base_call`). Complexity metrics get a
  GDScript branch/loop/return query, and `is_import_line` recognizes GDScript's
  actual dependency syntax — `extends`, `preload(...)`, and `load("res://...")`
  — since the language has no `import`.

  GDScript sits in the same tier as Zig: indexable but **not** structurally
  matchable, because `ast-grep-language` ships no GDScript grammar. `tsift
  ast-grep` and `structural_rewrite` do not reach `.gd`, and there is no
  semantic-edit executor for it.

- **All 20 structural-only languages are semantic-edit executors.** go, cpp,
  csharp, dart, java, swift, ruby, php, scala, lua, elixir, haskell, nix,
  solidity, css, html, json, yaml, hcl, and c now reach `tsift edit-intents`
  through `structural_rewrite`, with the full planner contract: patch proposal,
  bounded diff preview, `expected_content_hash` conflict detection, `--verify`
  in a detached temp worktree, batch rollback. 29 registered executors, up
  from 9.

  The recorded blocker for this was "each needs per-language tag queries and
  symbol extraction in `tsift-graph`/`tsift-search` first, because
  `parse_semantic_edit_source` validates through `executor.graph_lang()`." The
  first half of that sentence does not follow from the second. Reparsing a
  rewritten buffer needs a **parser**; tag queries and symbol extraction are
  what *indexing* needs. Routing the reparse through `graph::Lang` made a
  navigation capability a precondition for a write capability that never
  depended on it.

  The contract's `graph_lang` is now `Option<graph::Lang>`, and the reparse
  grammar resolves in two steps: an indexed language keeps using its
  `tsift-graph` grammar (Markdown in particular parses through `tsift-md-ast`,
  not ast-grep's `tree-sitter-md`), and everything else reparses with the same
  ast-grep grammar its pattern matched against. `AstGrepLang::tree_sitter_language()`
  exposes that grammar. An executor with neither is a registration bug, refused
  by name rather than parsed with another language's rules.

  These languages remain **unindexed, unsearchable, and ungraphable** — that
  tier is unchanged, and the symbol-resolved kinds (`rename_symbol`,
  `replace_function_body`, `insert_import`) stay unrecognized for them and are
  refused before the family split, which is what keeps a Go `rename_symbol` from
  being rewritten by Rust identifier rules.

  Coverage is a conformance table with one fixture per registered executor,
  exhaustive in both directions, driving the real planner path for all 29. The
  suite compares the sum of replacements *the planner returned* against the sum
  the table declares, so it cannot report green over a table that rewrote
  nothing. Grammar quirks stay row data: C and CSS need a statement terminator,
  Dart and Solidity match only whole declarations, HCL only as an attribute,
  JSON needs both sides metavariable-shaped.

- **Kotlin and Bash are semantic-edit executors (structural-only).**
  `structural_rewrite` needs only a grammar to match with and a grammar to
  reparse the result, so a language does not need per-kind rewriting to be a
  useful executor. Kotlin and Bash already had both an ast-grep grammar and
  graph symbol extraction, so `tsift edit-intents` now reaches them with the
  full planner contract — patch proposal, bounded diff, `expected_content_hash`
  conflict detection, `--verify` in a temp worktree, batch rollback. Their
  recognized-intent set is `structural_rewrite` alone; the symbol-resolved kinds
  stay unrecognized until their per-language rewriting exists.

  This exposed a fall-through worth naming: the family split routes anything
  that is neither markdown nor script to the **Rust** implementations, so a
  Kotlin `rename_symbol` was rewritten by Rust identifier rules and reported as
  "applied through the Kotlin executor" — plausible output from the wrong
  language's logic, and the catch-all message even called it "the Rust executor
  yet" on a Kotlin file. An executor that does not recognize a kind is now
  refused *before* the split, naming the executor and its supported kinds. The
  regression test asserts both halves: the codemod applies, and the
  symbol-resolved kind is refused without writing.

- **A mixed-language tree can be scanned with a pattern that is not valid
  everywhere.** Extending the conformance table to drive `scan`/`codemod`
  surfaced this immediately: a pattern is only ever valid for *some* grammars —
  `foo($A);` parses in the C family and is `MultipleNode` in every language with
  no statement terminator — and a walk picks a grammar per extension, so a mixed
  repository is the normal case. One grammar's refusal aborted the entire scan
  (and, before the `Pattern::try_new` fix below, panicked it), so a tree with a
  single `.py` file could not be swept with a C-style pattern at all.

  A file whose language cannot compile the pattern is now skipped and counted in
  `files_skipped_pattern_unsupported`, with the languages in
  `pattern_unsupported_langs`; both the human line and the JSON envelope report
  it, so a partial sweep never reads as exhaustive. When *every* scanned file was
  skipped that way the scan is an error rather than an empty result — a typo'd
  pattern must not report a confident "0 matches".

  ```
  $ tsift ast-grep search 'foo($A);'
  a.rs:2:4: foo(1);
  1 match(es) in 1 of 3 scanned file(s) [2 file(s) skipped: pattern does not parse as go, python]
  ```

  The file-scan conformance rows also pin extension dispatch (each fixture
  resolves to its own language with the same match count the buffer search
  produced), that non-source files are never parsed, that preview leaves every
  language byte-identical, and that `--apply` on one named file leaves the other
  27 languages untouched.

- **Cross-language conformance suites, and the two defects they found.** Both
  language tiers were covered by hand-written per-language tests, which can only
  prove that the language someone remembered works — a new grammar could arrive
  with no test at all, and no test checked the invariants that must hold
  *identically* everywhere.

  `packages/tsift-astgrep/tests/conformance.rs` now holds one fixture row per
  `AstGrepLang` variant (28) and `packages/tsift-graph/tests/conformance.rs` one
  per `Lang` variant (10), each run through a shared invariant set. A guard
  asserts the fixture set equals the language set, so adding a grammar without a
  fixture fails; rows must declare at least one match, and the suites assert the
  results the library actually produced sum to what the tables declare, so a
  suite over zero or vacuous rows cannot report green. Grammar quirks are row
  data (`granularity`, `known_non_matching`) rather than prose, so a grammar
  upgrade that removes a limit fails instead of leaving a stale note.

  - **An unparseable pattern aborted the process, on every language.**
    `ast-grep-core`'s `&str`-as-matcher path builds patterns with
    `Pattern::new`, which `unwrap()`s the parse. A pattern that was merely
    invalid for the selected grammar — two statements, `"a": $V` in JSON —
    panicked the CLI instead of erroring, and the pattern comes straight from
    the user. `search_source`/`rewrite_source` now compile with
    `Pattern::try_new` up front and surface the grammar's own message.
  - **Kotlin call edges were silently always empty.** `call_query()` named
    `simple_identifier`, a node type from the older `tree-sitter-kotlin` grammar
    that does not exist in `tree-sitter-kotlin-ng`. `Query::new` failed, the
    indexer downgraded it to a warning, and every Kotlin file produced zero call
    edges — so `graph --callers/--callees` and `explain` returned nothing for
    Kotlin without ever failing. Fixed to `identifier` /
    `navigation_expression`, with a behavioural regression test.

  Two documented pattern quirks came out of the sweep as well: HCL has no
  expression statements, so a call only matches as `$K = foo($A)`; JSON needs
  both sides metavariable-shaped (`$K: $V`).

- **Code Navigation split into a router block plus a generated runbook, and no
  longer duplicated into a `CLAUDE.md` that already imports `AGENTS.md`.**
  `tsift init` used to inject one ~4 KB block and then inject the same block
  again into `CLAUDE.md`. In the common Claude Code layout — `AGENTS.md`
  canonical, `CLAUDE.md` consisting of `@AGENTS.md` plus local notes — that put
  two verbatim copies of the same instructions into one prompt.

  The block in `AGENTS.md` is now a hot path: session start, the
  envelope-over-raw-read substitutions, verification. Everything else — budgets,
  `tsift workflow search`, `report.scale_guard` handling, the
  `tsift rewrite --run` path for harnesses without `PreToolUse` hooks, Codex and
  OpenCode integration — moves to a generated `runbooks/code-navigation.md`
  under its own `<!-- tsift:code-navigation-runbook v=X.Y.Z -->` markers. The
  block roughly halves.

  This does not reintroduce the runbook-only install hazard that kept the block
  self-contained before: `tsift init` **writes** the runbook, so the pair always
  ships together, and `tsift status` reports `instructions: stale` when the
  runbook is missing or its marker version differs — a repository initialized by
  an older tsift is repaired by the `tsift init` / `tsift status --fix` it
  already recommends. Text outside the runbook markers is preserved.

  De-duplication is deference-aware rather than unconditional. A `CLAUDE.md`
  that imports `@AGENTS.md` gets no section, and an existing managed section
  there is **removed**. A `CLAUDE.md` that is a symlink to `AGENTS.md` is left
  untouched — rewriting through the link would have stripped the section out of
  the canonical file it points at. A `CLAUDE.md` that stands alone still gets
  the section as before.

- **20 new structural languages (`tsift ast-grep`).** `ast-grep-language` ships
  28 grammars; tsift compiled 7 of them. Adds the remaining 20: c, cpp, csharp,
  css, dart, elixir, go, haskell, hcl, html, java, json, lua, nix, php, ruby,
  scala, solidity, swift, yaml. `tsift ast-grep search|rewrite` now works across
  most of the lazily binding set (go, cpp, csharp, dart, java), which is exactly
  the "same change in nine repos" shape structural patterns are good at.

  These are a deliberate **structural-only** tier: their `lang-*` features fan
  out to `tsift-astgrep` **only**, not to `tsift-graph`/`tsift-search`. A
  tree-sitter grammar is enough to match and rewrite a shape, but indexing
  additionally needs per-language tag queries and symbol extraction. So these
  languages are matchable and rewritable but **not** indexed, not searchable,
  and have no semantic-edit executor — which also means `structural_rewrite`
  refuses them, since it needs an executor to reparse and validate its output.
  `lang-zig` is the mirror image: indexable, but no ast-grep grammar. Promoting
  a structural-only language means adding the graph/search side first; the edit
  intent then follows for free.

  Aliases follow ast-grep's own `impl_aliases!` table so a pattern written
  against ast-grep's docs resolves identically here. `.h` resolves to C
  (ripgrep/tree-sitter convention); pass `--lang cpp` to force C++ headers.

  **Four grammars need patterns spelled differently, and this was found by
  probing each new language live rather than assuming the tier was uniform.**
  C and CSS need a trailing semicolon (`foo($A);`, `color: $V;`) — without it
  tree-sitter reads the fragment as a declaration and matches nothing. Dart and
  Solidity cannot parse a bare expression fragment at all, so they match only at
  declaration granularity (`void main() { print($A); }`); **Dart therefore does
  not support call-site codemods**, which is worth flagging because Dart was one
  of the languages this tier was added for. The other 16 match expression-level
  patterns directly. Each quirk is pinned by a test so an ast-grep bump cannot
  silently invalidate the documented workaround, and a companion test asserts
  go/cpp/java still match call expressions directly — the case the whole tier
  exists to serve.

  Two new drift guards, because the enum now has four parallel `match` arms and
  28 variants: one asserts every listed language is reachable from some file
  extension (a language added to `all()`/`from_name` but forgotten in
  `from_path` would be `--lang`-selectable while every directory walk silently
  skipped its files), and one asserts every variant maps to the correspondingly
  named `SupportLang` (a mismap is a wrong-grammar parse, which is worse than a
  refusal because it silently under-matches). Both were mutation-checked; the
  pre-existing name round-trip test stays green under those mutations, which is
  what shows the new guards cover arms it could not reach.

- **New `structural_rewrite` semantic edit intent.** `tsift edit-intents` now
  accepts a pattern-driven codemod kind that carries `file`, an ast-grep
  `pattern`, and a `replacement` template instead of a resolved symbol. This is
  the missing link between `tsift ast-grep rewrite` (which writes straight to
  the working tree with no reindex, impact report, or temp worktree) and the
  symbol-resolved intent kinds: a structural codemod now earns the same
  `--verify` proof, patch proposal, bounded diff preview, formatter policy,
  `expected_content_hash` conflict detection, and batch rollback that
  `rename_symbol` does.

  The kind is promoted across every registered executor at once — Rust, Python,
  TypeScript, TSX, JavaScript, JSX, and Markdown — because its selection and
  mutation logic are language-independent: the grammar is a parameter, not a
  code path. Language resolution goes through the file's executor contract `id`
  to an ast-grep grammar, so an executor with no grammar compiled into the build
  is an up-front refusal that names the structural languages that are.

  Fail-closed contract: `pattern` is required by `structural_rewrite` and
  **refused on every other intent kind**, so it cannot be silently ignored by an
  intent that does not read it; input and rewritten buffer are both reparsed
  with the executor grammar (the rewrite template is raw text, so that output
  reparse is the only guard against a template typo emitting unparseable
  source); and both degenerate outcomes are refusals rather than empty plans — a
  pattern that matched nothing, and a template that reproduces every match. The
  standalone `ast-grep rewrite` surface merely flags the latter `unchanged`;
  here a plan that cannot mutate must not be applyable.

  Covered by 9 unit tests (multi-match, non-Rust executor, multibyte source,
  both refusals, output-reparse refusal, validation symmetry, and a drift guard
  asserting every registered executor resolves an ast-grep grammar) plus 4 CLI
  integration tests (apply, dry-run-writes-nothing, no-match refusal, and
  `--verify` reaching `temp_applied_total > 0` with the real tree byte-identical).
  Spec: `specs/ast-edits.md` "Structural Intents" and
  `specs/structural-patterns.md` "Edit-intent integration".

## 0.1.78

- **lazily 0.49.0.** Picks up the `SourceMap` / `ComputedMap` keyed-collection
  rename (`#lzcellkernel`): the map entry nodes are `Source<V>` and
  `Computed<V>`, so `CellMap` / `SlotMap` were the pre-kernel names. This also
  drops the last consumer of the ancient `lazily 0.10.3`, which the published
  `tsift-core` had still been pulling into every downstream lockfile.

- **New `tsift ast-grep` structural search and rewrite (`tsift-astgrep`).** Adds
  pattern-shaped code retrieval and codemods on top of the tree-sitter grammars
  tsift already compiles: `tsift ast-grep search '<pattern>'`,
  `tsift ast-grep rewrite '<pattern>' '<replacement>' [--apply]`, and
  `tsift ast-grep languages`. Patterns use ast-grep metavariables (`$A`,
  `$$$ARGS`) and report path, 1-based line/column, byte range, matched text, and
  captures, in text, `--json`, or `--envelope` form. Rewrite **previews by
  default** and refuses `--apply` under a preview budget so a capped scan can
never land a partial codemod and report it as complete. Language resolution is
gated per compiled grammar, so an uncompiled language fails up front instead of
panicking inside the engine; `lang-zig` intentionally forwards nothing because
`ast-grep-language` ships no Zig grammar. Spec:
[specs/structural-patterns.md](specs/structural-patterns.md).
- **Workspace status auto-fix is scope-lazy.** A stale workspace now refreshes
  only submodule scopes reported stale or missing instead of rescanning every
  configured submodule because one scope changed.
- **Upgrade `lazily` 0.32.0 → 0.48.1.** All five reactive-core consumers now use
  the Cell-kernel API (`Source` / `Computed` and unified `get` / `set`) from the
  latest published dependency release.

## 0.1.77

- **External JSONL transcript targets now inherit their project root from the transcript `cwd`.** `session-review` and `context-pack` probe the bounded transcript header for Claude's top-level `cwd` or Codex's `session_meta.payload.cwd` before resolving index, diff, status, and graph work. A transcript under `~/.claude` / `~/.codex` can no longer fall back to the home directory and auto-index it indefinitely; the exact 822KB Claude transcript that previously pegged one CPU for more than fourteen hours now completes against its actual `boost-client` project.
- **Upgrade `lazily` 0.21.6 → 0.32.0.** The five reactive-core consumers use the current compatible dependency release included on main after 0.1.76.

## 0.1.76

- **Upgrade `lazily` 0.10 → 0.21.6.** All five lazily consumers (`tsift-core`,
  `tsift-graph`, `tsift-index`, `tsift-status`, `tsift-summarize`) now build against
  the current published `lazily` (0.21.6); the reactive core API (`Context`,
  `CellHandle`, `SlotHandle`) is unchanged, so this is a dependency refresh with no
  behavior change. Removes the stale `lazily 0.10.3` from the dependency tree for
  downstream consumers (e.g. agent-doc).

## 0.1.75

- **graph-db refresh CPU guard**: root-scale `tsift graph-db --path . --json refresh` no longer scans the entire indexed symbol table for every AST span. The traversal graph builder now pre-groups `all_symbols()` by file and passes only same-file symbols into AST parent/child and Markdown section metadata, removing the quadratic full-index walk that could peg one CPU indefinitely on large workspaces while still preserving AST navigation edges.

## 0.1.74

- **ci: clear GitHub Actions Node.js 20 deprecation.** `actions/checkout@v4` and `actions/cache@v4` target the deprecated Node.js 20 runtime — CI runs emit a deprecation annotation and are force-upgraded to Node 24, which breaks once GitHub fully removes Node 20. Both are bumped to `@v5` (the first major shipping on Node 24) across `ci.yml` and `release.yml`. `actions/upload-artifact@v4` / `download-artifact@v4` (release.yml only) are intentionally left as-is — their `v5` still runs on Node 20, and they were not flagged.
- **`tsift search --path` is now repeatable for multi-path search** (follow-up to `#ve5f`). The `Search` command's `--path`/`-p` argument was `Option<PathBuf>`, so a second `--path` failed with a clap "cannot be used multiple times" error and agents fell back to `rg` for cross-file lookups. It is now `Vec<PathBuf>` (repeatable), and the whole search path threads the list through: `cmd_search_with_budget` derives its `base_path` from the first entry (root resolution, submodule inference, and precheck unchanged) and collects every provided path that canonicalizes to a strict sub-directory of the root into `path_scopes`; the FTS/lexical result set is pruned to the **union** of those scopes (`prune_hits_to_path_scope` now takes `&[PathBuf]` and retains a hit under any scope), while exact search forwards all paths to ripgrep (`exact_search_command`/`run_exact_search`/`run_exact_search_with_timeout` now take `&[PathBuf]`; root display uses the first path). Single-path and no-path behavior is unchanged (empty `path_scopes` preserves the whole-index default; exact search with no path still passes `.`). The public `cmd_search` wrapper keeps its `Option<PathBuf>` signature (test call sites unchanged) and converts to a single-element vec internally. New unit test `keeps_hits_under_any_of_multiple_scopes` covers the union prune; the three existing `prune_path_scope_tests` were updated to the slice signature. Spec: `specs/search-navigation.md`.

## 0.1.73

- **Degraded-read-only fallback engine decided: keep `TokenIndex::build`** (#015t Phase 4b(a); operator, 2026-06-20). Resolves the open Phase 4b question by keeping the in-memory `TokenIndex` live-rebuild as the degraded-read-only fallback rather than replacing the call site with a literal `rg -F` walk and deleting the type. Both are equally *live* (no persisted cache survives — `token-index.json` was deleted), so the decision is about parity, not freshness: the rebuild preserves the tokenized OR-union matching and token-overlap ranking of the FTS path, keeping the transient degraded window behaviorally indistinguishable from the healthy FTS path, whereas `rg -F` would silently switch the fallback to literal substring matching with no ranking. No behavior change (status quo confirmed); recorded durably in the `TokenIndex` doc comment + `specs/search-navigation.md` so the type is not later removed as apparent dead weight. With Phase 4b(b) (`#ve5f` path-prune) shipped and (a) decided, #015t Phase 4b is complete; only the operator-gated Phase 5 crates.io release remains.
- **`--path` now sub-narrows FTS/lexical search hits** (#015t Phase 4b enhancement, `#ve5f`). The FTS5 `content_fts` path searches the whole project index regardless of `--path` (its stored paths are absolute), so a `--path <subdir>` argument previously scoped only the symbol prepass, never the lexical result set — unlike `--exact`, where `rg` already runs inside the sub-path. `cmd_search_with_budget` now captures the canonical sub-scope when `--path` strictly descends below the resolved project/workspace root and prunes the non-exact, non-federated result set to hits resolving under it (`prune_hits_to_path_scope`). Each surviving hit keeps its original BM25 `rank`, so the pruned set is a strict subsequence of the global ranking (narrowing changes which files appear, never their order). No-op for `--exact` (already scoped) and skipped for `--federated` (a single sub-path must not drop cross-repo hits); a `--path` at or above the project root preserves the whole-index default. Unit-tested in `tsift-cli` (`prune_path_scope_tests`): absolute-path narrowing with a sibling-prefix guard (`src/foobar` is not under `src/foo`), root-relative join, and empty result when no hit is under scope. Spec: `specs/search-navigation.md`.
- **Deleted the `token-index.json` persistence** (#015t Phase 4b). The legacy JSON `TokenIndex` is only ever reached as a **live fallback** (degraded/stale root `index.db` held by a concurrent writer, `--no-autoindex` on an un-indexed root, or direct programmatic callers), and that path must return current results. The old cache was keyed on file *existence* only — `load_or_build_token_index` returned `token-index.json` whenever it merely existed, with no mtime/content invalidation, so once written it served stale matches forever (files added or modified afterward were silently missing). Caching a must-be-live path was wrong by construction, so the persistence was removed: the fallback now always rebuilds the inversion in-memory. `TokenIndex::save`/`load` (the JSON persistence API) and the `Serialize`/`Deserialize` derive are removed; the generic `cache_dir` engine hook (and the `__search-worker --cache-dir` protocol) is retained for a future, properly-invalidated cache but no longer backs the token index. Regression test `search_reflects_files_added_after_first_search` proves a file created after the first search is now found. The companion Phase 4b decision — keep `TokenIndex::build` as the in-memory degraded-read-only fallback vs. replacing it with a live `rg -F` walk — is left as an operator call; this change keeps the engine and removes only its broken cache.
- **BREAKING CHANGE: FTS5 `index.db` search is now the DEFAULT lexical path** (#015t Phase 4 cutover). `tsift search --strategy lexical` (and the auto-resolved lexical default for multi-token queries) now runs the `index.db` `content_fts` FTS5 BM25 path instead of the JSON `TokenIndex`, returning `strategy: "fts"`. The normal flow's `precheck_search_indexes` + autoindex guarantees a fresh root `index.db` before search, so the BM25 path is always available; **ranking shifts from substring-position to BM25 by design** (the Phase 3 soundness gate proved candidate coverage, FTS ⊇ TokenIndex, so no hit is silently dropped — only the order changes). The JSON `TokenIndex` is **demoted to a fallback** for the only remaining cases that reach the engine without a root index.db (an un-indexed root reached with `--no-autoindex`, where the precheck otherwise degrades a *missing* index to exact search; and direct programmatic callers). Transition escape hatch: `TSIFT_FTS_SEARCH=0` (`0`/`false`/`no`/`off`) forces the legacy `TokenIndex` path. Exact-identifier lookups (`rg -F`) are unchanged. Follow-ups (Phase 4b): fully retire the `TokenIndex` build/cache + `token-index.json` once the no-index path auto-indexes, and add a `files_in_path_scope` zone-map prune for path-filtered queries.
- **FTS5 query path wired into the CLI** (#015t Phase 3b). New `sift::fts_search(db_path, root, query, limit)` builds a `SearchResponse` from the `index.db` content hits: BM25 owns file ordering, the lexical `score_file` picks each file's representative line + snippet from the **inline FTS body** (no disk re-read) — "BM25-vs-substring top-K reconciliation". New `IndexDb::content_fts_search_with_body` returns the inline body alongside the BM25 score so the CLI can reconcile ranking with substring line selection.
- **FTS5 query engine + soundness gates** (#015t Phase 3 core). New `IndexDb::content_fts_search_pruned(query, kind, limit)` — the consumer the `file_zonemap` substrate was built for: runs the BM25 MATCH then drops any hit the zone-map *proves* cannot contain the requested symbol kind (`files_possibly_containing_kind`), with the same soundness guarantee (a file lacking a zone-map row is retained, so the pruned set ⊆ unpruned and never drops a true match). New `sift::fts_match_query` translates a free-text query into an FTS5 `MATCH` that preserves the `TokenIndex` **OR-union** semantics (FTS5 would otherwise treat a multi-token string as an adjacency phrase), so the FTS result set is a **superset of the TokenIndex candidate set**. Two soundness-gate tests prove it: prune ⊆ unpruned / no-false-negatives, and FTS ⊇ TokenIndex candidates. Still **written-but-unread by the CLI** — wiring the env-flagged search path + `SearchResponse` construction (line numbers/previews/ranking reconciliation) is the remaining Phase 3b slice before the default can flip.
- Scaffolded a native SQLite **FTS5 content index** (`content_fts`) in `index.db` (#015t Phase 1/2) — the substrate that will replace the parallel JSON `TokenIndex`. One row per recognized-language file, `body` stored inline with the `unicode61` tokenizer (matches the current TokenIndex tokenization, so exact-identifier lookups via `rg -F` are unaffected). Maintained transactionally alongside `symbols`/`file_zonemap`: upserted at index time, cleared on modify, removed on delete, repopulated on `rebuild`. A `meta` `content_fts_version` stamp records the schema. **Written-but-unread** — the lexical search path still uses the JSON `TokenIndex`; the Phase 3 cutover (behind a flag, with a `pruned == unpruned` + top-K parity gate) will wire `content_fts_search` in. New `IndexDb` read API: `content_fts_count`, `content_fts_search` (BM25-ranked). Phase 0 spike findings (tokenizer choice, inline-body vs contentless ~+12MB index growth, recognized-language-file scope vs TokenIndex's all-candidate-files scope) recorded in `agent-loop/tasks/software/plan-tsift-fts5-content-index.md`.
- Added a per-file zone-map / min-max segment-statistics table (`file_zonemap`) to the index — the DuckDB row-group "zonemap" analog for pre-scan pruning. Each recognized-language file records its line span (`min_line`/`max_line`), `symbol_count`, the distinct symbol `kinds` present (comma-fenced for `LIKE`-based skip predicates), and `content_hash`. Populated incrementally at index time and on `rebuild`, cleared on modify, removed on delete.
- New `IndexDb` read API: `zonemap_count`, `file_zonemaps`, and `files_possibly_containing_kind` — a **sound** kind-filter prune: files lacking a zone-map row (e.g. a legacy index not yet reindexed) are conservatively retained, so the pruned set is always a superset of the true match set.
- Additive and behavior-preserving — no search path consults the zone-map yet. The lexical `Sift` search uses its own `TokenIndex` (not `index.db`), so live query-time pruning (plan Phase 3) is deferred pending a design that targets the index.db-backed path/kind/scope consumers. See `agent-loop/tasks/software/plan-tsift-zonemap-segment-stats.md`.

## 0.1.72

- **release: publish-list completeness for `tsift-local-model` + `tsift-kg`.** The v0.1.71 crates.io publish failed (`no matching package named tsift-kg found`, required by `tsift-memgraphrag`) because the two new KG crates were never added to `release.yml`'s two `for package in` publish lists, so they were never published and the dependency-ordered publish of `tsift-memgraphrag` broke. Both `tsift-local-model` (leaf) and `tsift-kg` (depends on `tsift-core`/`tsift-local-model`/`tsift-sqlite`, consumed by `tsift-cli`/`tsift-memgraphrag`) are now listed before `tsift-memgraphrag` in both the package-file check and the crates.io publish loop. **Operator gate:** these two crate names have no crates.io Trusted Publishing config yet (only the prior 23 crates do), so the OIDC publish of a brand-new crate needs a Trusted Publishing config created for each name on crates.io before the `v0.1.72` tag is pushed.
- **`kg` promotion into `tsift status` + `tsift workflow`** (`6af8a07`, previously unreleased): `tsift status` `use:` appends `kg` when a project `.tsift/graph.db` exists (omitted otherwise so agents extract before they read); new `tsift workflow kg` recipe (aliases `knowledge-graph`, `kg-workflow`) smoke → extract → status → refresh → evidence; unknown workflow topics fail closed listing `search, kg`. Spec: `specs/search-navigation.md`.

## 0.1.71

- **`#kgconfgate`**: provenance-aware `min_confidence` gating in the GraphRAG context pack. `#kgconfrank` made *ranking* trust real scores over derived defaults, but the gate still filtered by raw confidence, so a derived `default 0.500` survived a `min_confidence=0.4` gate while a real model `0.300` was excluded. `ContextCandidate` now also carries `confidence_is_default` (set only when `confidence_source=default` explicitly, distinct from `confidence_is_model`); `passes_confidence_gate` excludes explicit derived defaults outright under any positive `min_confidence`, while model-sourced and untagged/legacy (pre-`#kgconf`) nodes keep raw gating and `min_confidence==0` admits everything. Spec: `specs/local-kg-model.md` § GraphRAG Context Retrieval.
- **`#kgsameas`**: durable `same_as` edges for canonical-entity duplicates so graph-level consumers (`tsift graph`/`explain`/`summarize`, the SurrealDB projection) collapse them too — `#kgentitycollapse` deduped only the context-pack retrieval surface. New `SqliteGraphStore::link_nodes_by_shared_property` stars nodes of a kind sharing a prefixed id-property value to the group's smallest node id with link edges of a given kind (idempotent: edges key on from/to/kind; no node deleted, provenance preserved); `tsift-kg::link_canonical_entities_sqlite` wraps it for `semantic_entity` / `entity_id` / `kgent-` / `same_as`. `kg extract` runs the linker after the per-source replace upsert. Spec: `specs/local-kg-model.md` § GraphRAG Context Retrieval.
- **`#kgconfrank`**: rank model-sourced confidence above derived defaults in the context pack. `#kgconf` records a `confidence_source` tag but the ranker read only raw confidence, so a derived `default 0.500` outranked a real model `0.300`. `build_context_pack` now inserts a confidence-provenance tier (`confidence_is_model`) between connectivity and raw confidence — model-sourced ranks above derived-default at equal degree; within a tier raw confidence still decides. Spec: `specs/local-kg-model.md` § GraphRAG Context Retrieval.
- **`#kgentitycollapse`**: query-time identity merge in context-pack candidate collection. The extractor reconciles a recurring entity to a canonical `kgent-…` id (the `entity_id` property), but each chunk/source still projects a distinct node id, so the graph holds duplicate canonical entities. `collect_candidates_from_nodes` now collapses candidates sharing a canonical `entity_id` to one representative (highest confidence → group max degree → smallest node id); read-side only (no node deleted), and chunk-local slug ids (`e0`/`e1`, not `kgent-`) are never merge keys. The bounded scan now counts nodes scanned (not deduped output). Spec: `specs/local-kg-model.md` § GraphRAG Context Retrieval.
- **`#kgrefreshdup`**: `tsift kg extract` / `refresh` no longer accumulate duplicate canonical entities. The prior upsert was purely additive, so re-extracting an edited source (shifted chunk byte-ranges → changed chunk + entity node ids) left the old nodes orphaned while new ones piled on (observed 9 → 18 entities for one source after one refresh). New `SqliteGraphStore::delete_source_projection(source_ref, provider)` deletes a provider's prior nodes for a source (cascading edges/properties/vectors via `ON DELETE CASCADE`, provider-scoped so it never touches AST nodes); `replace_kg_source_projection_sqlite` deletes-then-upserts per source, making extraction idempotent. Distinct sources untouched. Spec: `specs/local-kg-model.md` § On-Demand Extraction Refresh.
- **`#kgconf`**: the KG projection always persists per-entity/relation `confidence`. Local-model structured output drops the optional field, so 0 of freshly-extracted `semantic_entity` nodes carried confidence and the spec'd confidence/recency gating had no data. `entity_node`/`relation_edge` now always write a `confidence` property (model value or derived `DEFAULT_KG_CONFIDENCE = 0.5`) plus a `confidence_source` tag (`model`|`default`); the Ollama structured-output schema marks `confidence` required to elicit real scores. Spec: `specs/local-kg-model.md` § Graph Provenance.
- **`#kgctxinject`** (Phase 2 of `#kggraphscope`): the `#kgctxretrieve` known-entity pack is now injected into the KG extractor prompt so the local model reconciles against canonical `kgent-…` stable ids instead of re-inventing them. `OllamaKgExtractor::chat_request_body_with_context` prepends a bounded `[KNOWN ENTITIES …]` block (`stable_id | label | kind | confidence`, one line per ranked entity) ahead of the chunk under a `[CHUNK]` marker; with no/empty pack the request body stays byte-for-byte identical to the plain path. `ChunkContextSource` derives deterministic per-chunk seeds (`derive_seeds`: alphanumeric tokens length > 2, lower-cased, de-duped, first-appearance order, `max_seeds`-capped) and builds each chunk's pack; the new `KgExtractor::extract_json_with_context` defaults to the plain `extract_json` so every existing extractor is backward-compatible, and `extract_documents_to_projection_with_context` threads the pack through. CLI: `tsift kg extract --graph-db <db>` injects context automatically when the graph already holds `semantic_entity` nodes (opt out with `--no-context`); the extract outcome reports `context_entities`. New lib + CLI tests cover the context-aware prompt builder, seed derivation, the per-chunk source, and the threading seam. Spec: `specs/local-kg-model.md` § GraphRAG Context Retrieval.
- **`#kgctxincremental`** (Phase 3 of `#kggraphscope`): graph-aware incremental re-extraction. Because `tsift kg refresh --apply` re-extracts each changed source through the shared lease-aware `run_kg_extract` path, the `#kgctxinject` context-injection seam makes those re-extractions reconcile against the existing graph's stable ids instead of duplicating them (composes with `#kgextractrefresh`'s changed-file detection). `--no-context` opts out of the reconciliation. Delivered by composition over the Phase 2 seam — no new CLI surface beyond the shared `--no-context` flag. Spec: `specs/local-kg-model.md` § GraphRAG Context Retrieval.
- **`#kgctxretrieve`** (Phase 1 of `#kggraphscope`): deterministic, bounded GraphRAG known-entity-pack retrieval API in `tsift_kg::context_pack`, the foundation for feeding existing graph context into the KG extractor (Phase 2 `#kgctxinject` injects it into the prompt; Phase 3 `#kgctxincremental` reconciles re-extraction). Today `extract_documents_to_projection` extracts each chunk in isolation, so the model re-invents `kgent-…` stable ids and produces duplicate/variant entities + missed cross-chunk relations. New pure API: `collect_candidates_from_nodes` / `_from_projection` scan at most `ContextPackConfig::max_candidate_scan` `semantic_entity` nodes and compute degree from a single edge pass (never a full dump — same discipline as `#kgwiring`'s evidence cap), reporting `scan_truncated`; `build_context_pack` ranks the bounded set by seed match (token overlap) → connectivity → confidence → stable node-id tiebreak (Run Manifest determinism), confidence-gated + `max_entities`-capped. Fully pure + 8 fixture-graph unit tests. Spec: `specs/local-kg-model.md` § GraphRAG Context Retrieval. Plan: `tasks/software/plan-tsift-kg-graphrag-context.md`.
- **`#kgrefreshapply`**: `tsift kg refresh --apply` automatically re-extracts every stale / `no_recorded_hash` source identified by the `#kgextractrefresh` staleness plan, reusing the lease-aware `kg extract` path so concurrent refresh-extracts still serialize on one GPU. Operator-gated (loads the local model: needs GPU + Ollama). Sources whose `source_ref` is no longer a readable file are skipped. The per-source outcome is collected into a unified summary — a single JSON document in `--json` mode (`results`/`errors`/`skipped`) and a readable per-source block in human mode — instead of interleaving per-source stdout. New `refresh` flags: `--apply`, plus the extract pass-through `--profile` / `--model` / `--host` / `--no-lease` / `--idle-ttl-seconds` / `--keep-loaded` / `--lease-file`. Refactored `cmd_kg_extract` into `run_kg_extract` (returns `KgExtractOutcome`) + printer so the apply loop and standalone extract share one lease-aware path; the read-only plan now also hints `tsift kg refresh --apply`. Pure `RefreshPlan::apply_targets` selector with unit coverage. Spec: `specs/local-kg-model.md` § On-Demand Extraction Refresh.
- **`#kgextractrefresh`**: On-demand extraction-refresh trigger so `.tsift/graph.db` is not a silent stale snapshot. `kg extract` now records a `source_content_hash` (blake3) on each `kg_source` node, and the new `tsift kg refresh` command compares recorded vs current file hashes, classifying each source as `unchanged` / `stale` / `missing` / `no_recorded_hash` and printing the exact `kg extract` command to refresh each one. Read-only and model-free (cheap to run on demand or from a hook); pure `plan_refresh` planner with unit coverage. Spec: `specs/local-kg-model.md` § On-Demand Extraction Refresh.
- **`#kgleasewire`**: Wired the `#kgreflease` lease lifecycle into `tsift kg extract` so the cooperative GPU lease is actually consumed (previously the registry had zero callers outside the CLI/tests). Extract now acquires an exclusive lease for the resolved profile before loading the model (concurrent extracts serialize on one GPU; a conflict fails with the holding pid unless `--no-lease`), and on success releases it and unloads the model when it dropped the last reference (`--keep-loaded` to skip). A bailed extract leaves a pid-dead holder reclaimed by the next acquire/reap (crash-safe, not a leak). New `kg extract` flags: `--no-lease`, `--idle-ttl-seconds`, `--keep-loaded`, `--lease-file`; new `resolve_lease_profile_id` (explicit `--model` bypasses leasing). Spec: `specs/local-kg-model.md` § Multi-Session Reference-Counted Lease Lifetime.
- **`#kgreflease`**: Hardened the cooperative GPU lease registry into a robust multi-session reference-counted model lifetime. Acquire/release/renew/reap now hold an `fs4` OS advisory lock on a `<registry>.lock` sidecar across the whole read-modify-write, closing the TOCTOU lost-update race between concurrent processes (atomic temp+rename only made each *write* atomic). Added an explicit heartbeat (`renew_lease` / `tsift lease renew`) so a long-lived session slides its `idle_ttl_seconds` window forward — `acquired_at_unix_seconds` now doubles as the last-heartbeat anchor — alongside the existing pid-dead crash reclamation. Added `reap_leases` / `tsift lease reap` to sweep crashed (pid-dead) and TTL-expired holders and report `emptied_profiles` (profiles whose last reference was reclaimed). Reference-counted unload: `tsift lease release --unload-on-last-release` and `tsift lease reap --unload-empty` POST the provider-native `keep_alive:0` unload only when a profile's live holder count reaches zero, so the model stays loaded while ≥1 session references it and VRAM frees exactly when the last reference goes away. `fs4` added to `tsift-local-model`. Spec: `specs/local-kg-model.md` § Multi-Session Reference-Counted Lease Lifetime.
- **`#kgwiring`**: Wired the previously dormant `tsift-agent-doc` graph-evidence read seam into an active planning workflow. `session-digest` (and the digest-backed `session-review` / `context-pack` planning surfaces) now surface a bounded `graph_evidence` section from `.tsift/graph.db` — graph node/edge totals plus the most connected KG entities, scoped to the session's top touched symbol. A new `graph_evidence::read_graph_evidence_bounded` guards cost: it checks the cheap `graph_counts()` query first and reports `scanned: false` with totals only (no full `all_nodes()` scan) when the store exceeds `DEFAULT_EVIDENCE_MAX_SCAN_NODES` (50k), so a 418k-node workspace graph no longer loads into memory on every digest cycle. Missing `.tsift/graph.db` yields no section; KG read errors degrade to a digest `warnings` entry. Added a `scanned` flag to `GraphEvidenceReport` (additive in `tsift kg evidence` JSON). Spec: `specs/local-kg-model.md` § Evidence Surfacing in Session Digest.

## 0.1.70

- **`#cargoidxcov`**: `source-read`/`symbol-read` build the per-package cargo index on demand so symbol projection works for packages whose index had not yet been materialized.
- **`#tsreviewcleanup`**: Three `session-cost`/`graph-db` review cleanups across the prompt-cache attribution and snapshot-export paths.
- **prompt-cache fidelity**: track raw `stable_prefix` so explicit fingerprints don't bypass prefix-drift attribution; make breakpoint identity position-independent (`#pcachebp`); count read/create regression from the raw signal (`#pcacheregtrunc`); attribute prompt-cache drift to the concrete changed field (`#pcacheattr`).
- **graph-db write safety**: serialize graph-db writes + snapshot-import with an advisory lock (`#gdbwritelock`); cover `compact --apply` and memory upsert under the write lock (`#gdblockcover`); map snapshot-export locked-db to the live-lock diagnostic (`#gdbexportlockdiag`); record WAL checkpoint outcome instead of swallowing it (`#gdbwalcheckpoint`).
- **log-digest fidelity**: restrict cargo `<name>` fold to the canonical progress shape (`#logfoldname`); add a dropped-error fidelity guard to the gate (`#loggatefidelity`); surface distinct-error count when failures fold (`#logfoldloss`); classify canonical terminal-failure summary lines (`#logclassfn`).
- **`#cicacheguard`**: make the CI cargo cache step non-fatal.

## 0.1.69

- **`#lazilybump`**: Upgraded the `lazily` dependency requirement from `0.2` to `0.10` in `tsift-core`, `tsift-graph`, `tsift-index`, `tsift-status`, and `tsift-summarize`. The crates already build and lock against `lazily` 0.10.x locally via the superproject path patch; the published manifests now declare the matching `^0.10` requirement so crates.io consumers resolve the same line instead of the stale `0.2.x` series.

## 0.1.68

- **`#logfixturecov`**: Hardened the `log-digest` fixture gate with a false-positive guard. `forbidden_signals` now checks the *classified signal messages* (`signal_message_text`) instead of the full digest projection, because a benign line's tokens can still surface as a symbol or file ref even when the line is correctly not classified as an error. Added a `classifier-false-positive-precision` fixture case proving benign lines (`stderr!`, a `docs/err_pnpm-notes.md` path, a `err_pnpmish-fixture` symbol) are not misclassified as errors while the real `npm ERR!` / `ERR_PNPM_LIFECYCLE_FAIL` markers still classify, plus a unit test that the symbol-leak token is in the full projection but not the signal text.
- **`#logsigfp`**: Hardened the `log-digest` npm/pnpm error classifier against false positives. The previous `#loggate` change matched `err!` and `err_pnpm` as bare substrings, so benign lines containing the fragments (`stderr!`, a `/path/err_pnpm-notes` token, a URL) were misflagged as error signals. Detection is now whitespace-token-anchored: an `ERR!` token (npm/yarn) or a token prefixed `ERR_PNPM_` (pnpm). Added a regression test proving real markers still classify while `stderr!`, mid-token `err_pnpm` fragments, and `err_pnpmish` do not.
- **`#loggate`**: Added a fixture-backed token-savings + false-negative gate for `log-digest` via `tsift log-digest --fixture <file> [--fail-under]`, covering cargo, pytest, npm, pnpm, and agent-doc runtime logs. Each fixture case asserts the digest both compresses bulky near-duplicate noise (`minimum_savings_percent` measured against a deterministic ~4 chars/token estimate over the `digest_signal_text` projection) and preserves real failures (`required_signals` must survive into the digest; `forbidden_signals` must not appear). The generic signal classifier now also recognizes cargo `error[...]`, pytest `E   ` assertion detail, and npm/pnpm `ERR!`/`ERR_PNPM` error forms so those signals are no longer folded away as noise. Ships `fixtures/log-digest-token-savings.json` (5 ecosystems, 52–67% measured savings) and a `--fail-under` CI gate enforced by an integration test in `make check`.
- **`#logchunk`**: `log-digest` now persists bulky raw transcripts behind an artifact handle instead of losing stdin input or relying only on inlined groups. When the raw log is ≥4096 bytes or any signal/repeated-line/line-family/stack group overflows its inline cap, the standalone command writes the raw bytes to `.tsift/artifacts/<handle>.log` and attaches a `raw_log_artifact` ref (handle, relative path, byte/line counts, `tsift log-digest --input <artifact> --json` expansion command) to JSON/compact/human output; small logs attach nothing. `context-pack` surfaces the same `raw_log_artifact` shape on its bounded log preview, referencing the existing on-disk `--log-input` file rather than re-persisting it. Mirrors the existing `digest-runner --kind log` artifact-handle pattern.
- **`#logfam`**: Added semantic line-family folding to `log-digest`. Beyond exact-line collapsing, each transcript line is reduced to a template by replacing variable tokens — bracketed epoch timestamps, file paths, semantic versions, hex hashes, numbers/durations/sizes/percentages, the value side of `key=value` structured fields, and the cargo-style crate name before a bare version — with stable placeholders. Lines sharing a template fold into a `line_families` group carrying the template, occurrence count, distinct-variant count, and first/last raw samples. Only multi-variant families (cargo/build/install progress, dependency downloads, repeated harness lifecycle lines that vary by one path/count/timestamp/crate name) are reported, ranked by occurrences and bounded to the top families with the full `line_family_groups` count preserved. Surfaced in JSON, compact, and human `log-digest` output.

- **`#tsbuglock`**: Added graph-db doctor regressions for locked SQLite graph databases. The tests cover both rollback-journal snapshot recovery and WAL-aware sidecar snapshot recovery, ensuring doctor reports the `sqlite_graph_db_read_recovery` diagnostic without failing closed when read-only graph evidence can safely fall back to a temporary snapshot.
- **`#x5fw`**: Moved agent-doc session markdown/log recognition plus session-id, backlog, and queue row parsing into exported `tsift-agent-doc::session_markdown` helpers. `session-review`, traversal graph projection, and rewrite session-digest routing now consume the shared helpers instead of duplicating marker-level `agent_doc_session`, `agent:queue`, and backlog parsing in `tsift-cli`.
- **`#pcache-scorecard`**: Added a compact prompt-cache ROI scorecard to `session-cost` and `session-review`. `session-cost` now groups prompt-cache economics by provider with net cached-read tokens, read/create ratio, trend, suspected invalidation cause, and a rerun command; `session-review` carries the same rows per bounded matched session with exact `tsift session-cost --source ... --input ... --json` commands. JSON, human, and compact output all surface the scorecard when prompt-cache signals are present.
- **`#pcache-fixture-drift`**: Extended the real-session prompt-cache effectiveness fixture with required drift-mode coverage for volatile generated prefix headers, cold standalone compaction, OpenAI `prompt_cache_key` churn, and replica routing churn. `session-cost --fixture` now reports required/covered/missing regression scenarios and fails closed when required coverage disappears even if individual cache economics cases still pass.
- **`#token-actions-rewrite`**: `session-review --next-context` token actions now carry concrete `rewrite_commands` alongside compact/restart/digest commands. Prompt-budget and cached-resend guardrails point at bounded `context-pack` and `session-review` refreshes; repeated raw file/session/log reads add a `repeated_raw_read` action with `tsift rewrite --run ...` and `tsift --envelope source-read ...` replacements; repeated command bundles add a `repeated_command_bundle` action with exact `tsift rewrite --run ...` commands for the repeated shell calls. Human budget/context-pack output prints the new rewrite command category, and tests cover both unit budgeting and compiled CLI JSON output.
- **`#contextpack-queue-slim`**: Added an `agent_doc_queue` profile to `session-review --next-context` and composed `context-pack` output for agent-doc markdown targets. The bounded profile resolves the active queue head to its backlog prompt, carries live unresolved exchange-tail lines, top unchecked backlog/review rows, compact prompt preset refs, and expansion handles while omitting archived summaries, done history, and stale transcript context by default.
- **`#pcache-prefix-drift`**: Added a bounded prompt-cache prefix drift report to `session-cost`. Prompt-cache analytics now compare adjacent calls by stable-prefix fingerprint, prompt cache key, explicit breakpoint paths, routing affinity, and provider, then annotate cache-ratio drops or cache-creation spikes with the first changed field in JSON, compact, and human output.

## 0.1.67

- **`#kx6s`**: Added a MemGraphRAG performance baseline gate to `metric-digest`. Runs with `memgraphrag.<workload>.duration_micros` metrics now emit `memgraphrag_performance_gate`, requiring memory query, memory project-graph, `graph-db related`, and semantic seeded neighborhood latency evidence with a compared baseline and a 25% max latency regression. Added `fixtures/memgraphrag-performance-history.json`, CLI fixture coverage, and spec notes.
- **`#qszc`**: Indexed MemGraphRAG memory candidate retrieval. `tsift-memory` schema v2 now maintains observed/create-time indexes plus an FTS5 `memory_events_fts` index, and `tsift-memgraphrag` ranks bounded FTS/recent candidate sets instead of every stored memory event.
- **`#qs5v`**: Materialized semantic graph embeddings in SQLite schema v6 via typed `graph_node_semantic_vectors` blob rows and added `GraphStore::semantic_top_candidates`, so `semantic` / `graph-db related` seed retrieval no longer scans and parses every semantic node property on SQLite.
- **`#zkft`**: Made memory graph projection explicitly incremental and watermarkable. `tsift memory project-graph` now supports `--read-policy recent-first|oldest-first|query-relevant` (`query-relevant` requires `--query`), projects a `memory_projection` metadata node with source watermark/content hash/read-policy details, and reports those hashes in JSON. `tsift-memory` exposes matching policy reads and watermarks, while SQLite `upsert_projection` skips unchanged row hashes instead of rewriting graph rows and materialized property/vector tables on repeated memory projection runs.
- **`#58p8`**: Folded observed-at freshness decay and memory-node signals into `GraphStore::ranked_neighborhood`. Ranked neighborhoods now score projected memory events/source rows and authored finding/decision/note nodes through the same graph-pruning path as code nodes, with configurable `observed_at_*` and `memory_node_boost` options; the default store scores sibling candidates before pruning and SQLite mirrors the score expression in SQL.
- **`#kn0d`**: Pushed semantic-seeded neighborhood caps into `GraphStore`. `graph-db related` now calls `GraphStore::semantic_seeded_neighborhood` with shared edge-scan and node-discovery caps; the default store batches candidate node lookups per ranked expansion and SQLite ranks/limits incident/outgoing expansion edges in SQL before returning rows to Rust.
- **`#pcache-measure`**: Added a real-session prompt-cache effectiveness fixture gate to `session-cost`. `tsift session-cost --fixture fixtures/real-session-prompt-cache-effectiveness.json --fail-under --json` now runs self-contained Claude/Codex usage cases through the existing session-cost parser and fails closed on cached-input ratio, net cached-token, or read/create-regression threshold misses.
- **`#pcache-fingerprint`**: Added prompt-cache attribution metadata to `session-cost` usage samples and prompt-cache timelines. Claude/Codex calls now emit provider, optional prompt-cache key, stable-prefix fingerprint, explicit breakpoint metadata, and optional routing affinity in JSON plus compact/human output so cache misses can be compared against prefix/key/breakpoint changes.
- **`#pcache-adapters`**: Enforced provider-specific prompt-cache adapter evidence in `session-cost`. Prompt-cache plans now classify Anthropic `cache_control`, OpenAI `prompt_cache_key`, and replica-local routing affinity as observed/missing/partial/churned adapter states, add remediation actions for missing evidence, and make the prompt-cache fixture gate fail closed when the source-specific adapter state is not proven.

## 0.1.66

- Added a dedicated `tsift-memgraphrag` workspace crate for the MemGraphRAG graph/RAG layer. `tsift-memory` now stays focused on durable memory storage, capture contracts, and imports, while `tsift-memgraphrag` owns decay-weighted ranking/query plans, memory-event graph projections, `.tsift/memory.db` upsert into the shared graph store, traversal refresh memory/semantic rows, and ontology materialization. The root crate re-exports it as `tsift::memgraphrag`, and `tsift-cli` depends on it directly.
- Added a dedicated `tsift-cache` workspace crate and moved the cycle-packet cache implementation there, leaving `tsift_quality::cycle_packet_cache` as a compatibility re-export. CLI callers now depend on `tsift-cache` directly, and root `tsift` re-exports the crate as `tsift::cache` so agent-doc-facing consumers can pull shared cache primitives without taking the full quality-gate surface.
- `session-cost` prompt-cache diagnostics now include bounded effectiveness analytics: sample count, stable/improving/declining trend, average/first/last cached-input ratios, net cached-read versus creation tokens, optional read/create ratio, likely invalidation diagnostics for ratio drops, creation spikes, and read/create regressions, and a first-plus-latest per-turn cache timeline in JSON and text output.

## 0.1.65

- `tsift init` now keeps the generated AGENTS/CLAUDE Code Navigation block self-contained for Codex/OpenCode prompt reuse while pointing repositories that ship current `.claude/skills/tsift/SKILL.md` or `runbooks/code-navigation.md` back to those deeper runbooks, avoiding a runbook-only install that breaks standalone checkouts.
- **`#source-read-envelope-columns`**: `tsift --envelope source-read` now applies the existing schema-then-values transform by default, keeping JSON while emitting dense repeated lists such as `summary.metrics`, source `preview` lines, symbol refs, and Markdown outline nodes as `_c`/`_r` column tables. Non-envelope `source-read --json` remains the expanded object-array shape unless callers pass `--schema`. Updated the output-format spec, source-read envelope tests, and nested semantic-edit verification source-read parsing.
- **`#release-publish-md-ast`**: added `tsift-md-ast` to the dependency-ordered crates.io package-check and publish lists, and tightened the release guard test so every split crate in the publish order is checked in both workflow steps.

## 0.1.64

- **`#md-ast-incremental-reparse`**: `tsift-md-ast` now exposes `MdTextEdit`, `reparse_incremental()`, and `reparse_incremental_with_input_edit()` so CRDT/live-editor consumers can update a prior `tree-sitter-md` tree instead of reparsing each keystroke from scratch.
- **`#opencode-pack-check`**: local `make check` now validates `opencode-tsift` package contents with `npm pack --dry-run` instead of `npm publish --dry-run`, keeping CI green after the current package version already exists on npm. The release workflow still owns the publish dry-run before tagged npm publishes.
- **`#npm-oidc-trusted-publishing`**: switched the `opencode-tsift` npm publish job to OIDC trusted publishing — no long-lived `NPM_TOKEN` secret. Added `id-token: write` permission, bumped to Node 24 + `npm@latest` (OIDC needs npm CLI ≥ 11.5.1), and dropped `NODE_AUTH_TOKEN`. Requires a one-time manual bootstrap publish (npm has no "pending publisher", so OIDC can't create a brand-new package) plus a Trusted Publisher configured on the package (repo `tsift`, workflow `release.yml`, action `npm publish`).

## 0.1.63

- **`#release-publish-completeness`**: completed the crates.io publish set. `tsift-memory` (a dependency of `tsift-cli` added since 0.1.62) was missing from the release workflow's publish list, and `tsift-cli`'s optional `tsift-surrealdb` dependency pointed at a workspace-excluded, never-published crate — both blocked `cargo publish -p tsift-cli` with "no matching package found". Brought `tsift-surrealdb` into the workspace (un-excluded; clippy-clean, `tsift-core`-only dep) and added both `tsift-memory` and `tsift-surrealdb` to the release workflow's package-check and publish lists ahead of `tsift-cli`, so the full crate set (sub-crates + `tsift-cli` + root `tsift`) publishes and the `backend-surrealdb` feature resolves on the published crate.
- **`#digest-runner-public`**: promoted the previously hidden `__digest-runner` helper to a first-class public CLI command, `tsift digest-runner`. The command now shows in `tsift --help` with a description (`Run a shell command and emit a bounded, artifact-backed test/log digest envelope`); the old `__digest-runner` name stays as a hidden backward-compatible clap `alias`, so already-emitted rewrites, installed instruction files, and `.codex`/`.opencode` hooks keep resolving. All new emissions use the public name: the `rewrite` builder (`build_digest_runner_command`) now produces `tsift --envelope digest-runner ...`, and the `tsift init` instruction templates (Code Navigation block, OpenCode `tsift-test-digest`/`tsift-log-digest` commands) emit `digest-runner`. Updated `specs/search-navigation.md`, `specs/output-formats.md`, `specs/release-integration.md`, `AGENTS.md`/`CLAUDE.md`, and the committed `.opencode`/`opencode-tsift` command surfaces. Tests: the `exit_code.rs` digest-runner integration tests invoke the public `digest-runner` name, a new `digest_runner_legacy_underscore_alias_still_resolves` test locks the `__digest-runner` alias, and the `rewrite`/`sim_world`/`init` unit tests assert the emitted command now uses `digest-runner` (and no longer the underscore-prefixed form).
- **`#specsplit`**: Split the 1960-line `SPEC.md` monolith into behavior-bounded sibling specs under `specs/` (`architecture.md`, `graph.md`, `search-navigation.md`, `output-formats.md`, `digests-sessions.md`, `release-integration.md`), following the agent-doc `split-spec-files` runbook. `SPEC.md` is now a stable index: Goal plus a command/spec map linking each sibling, so external links and the `packages/tsift-cli/SPEC.md` symlink keep working. Content moved verbatim (every original line assigned to exactly one sibling). Updated the `exit_code.rs` release-publish and lazily-rs cache-contract tests to read the sibling specs, and repointed `cycle_packet_cache` / `token_gate` / `perf_gate` doc-comments at `specs/graph.md`.
- **`#trt1p2b`**: Findings Graph Layer hot-path injection extended to `search` and `explain` (completing Phase 2). `tsift explain <symbol>` folds trusted, fresh findings concerning its result set (focused symbol, displayed callers/callees, community members) into the JSON/envelope output (both budget and non-budget paths) and a non-JSON `Findings` section; `tsift search <query>` does the same for matched symbol names and their files. Reuses the Phase 2 `collect_injectable_findings` trusted+fresh contract via a new `collect_result_set_finding_previews` helper (`ResultSetFindingPreview`: `id`, `kind`, `title`, `about`, `anchor_kind`, optional `confidence`, budget-truncated `body`, `expand`); the `findings` array is capped (10), body-truncated (240 B), omit-when-empty, and fails open. Compiled tests in `packages/tsift-cli/tests/finding.rs` cover explain injection, search injection, and draft exclusion for both. Spec updated in `specs/graph.md` § Hot-path injection (Phase 2).
- **`#trt1p4`**: Findings Graph Layer Phase 4 (passive harvest) shipped — the layer is now feature-complete (Phases 1–4). New `tsift finding harvest` passively extracts `draft` candidate findings from `.agent-doc/archives/*.md`: for each line pairing a decision/insight signal (`decided`, `decision`, `invariant`, `gotcha`, `by design`, `fail-closed`, `source of truth`, …) with an inline-code token that resolves to an indexed symbol or real file (resolvable watermark required), it stores a finding anchored to that token (`kind` = `decision` for design-intent, else `note`). The whole path is **fail-closed**: it bails unless `.tsift/config.toml` sets `[findings] passive_harvest = true` (new `FindingsConfig` on `tsift_index::config::Config`, default off). Capture is generous (everything lands as `draft`, excluded from injection), bounded (`HARVEST_CAP`), idempotent (content-derived ids), and never overwrites an existing finding (`skipped_existing`). New `tsift finding promote <id>` flips a `draft` to `trusted` (preserving anchor/watermark/provenance), making it eligible for hot-path injection (#trt1p2) and map annotation (#trt1p3); idempotent on trusted, fails closed on unknown id. New `Harvest`/`Promote` `FindingCommand` variants + `cmd_finding_harvest`/`cmd_finding_promote` in `commands/finding.rs`; compiled tests in `packages/tsift-cli/tests/finding.rs` (fail-closed gate, draft extraction, idempotency, promotion, unknown-id) and `packages/tsift-index/src/config.rs` (flag default-off + opt-in). Spec updated in `specs/graph.md` § Passive harvest (Phase 4) and § Capture policy.
- **`#trt1p3`**: Findings Graph Layer Phase 3 (graph menu + exports) shipped. `tsift graph-db map` now annotates each community, hub, and `--focus` node with the trusted, fresh findings that `concern` its displayed members / label / symbol — reusing the Phase 2 `collect_injectable_findings` trusted+fresh contract, so `draft`, stale, and no-watermark findings never appear, matching is bounded to displayed names (not a full-store scan), and findings are deduped by id per node. The JSON `overview` communities/hubs and `focus` report each gain an omit-when-empty `findings` array. New `tsift graph-db map --format md|html` renders on-demand projections of the same overview + findings: `md` is greppable/commit-friendly (`📌 <kind>: <title> (about <anchor>)` bullets), `html` is a self-contained, escaped, styled page for an interactive human view. Both are pure projections — the graph store / JSON stays the single source of truth; without `--format` the command keeps its JSON/text output. New `MapFormat` CLI enum, `GraphDbMapFindingRef` + annotation helpers, and `render_graph_db_map_markdown`/`render_graph_db_map_html` in `commands/infra.rs`; compiled tests in `packages/tsift-cli/tests/graph_db_conformance.rs` cover hub annotation, md+html rendering, and draft exclusion. Spec updated in `specs/graph.md` § Graph menu + exports (Phase 3). Passive harvest remains as `#trt1p4`.
- **`#trt1p2`**: Findings Graph Layer Phase 2 (hot-path injection) shipped for `context-pack`. Trusted, fresh findings whose `about` anchor matches a node already in the context-pack result set (touched files/symbols, diff-preview files + touched symbols, and exploration source-window files + relationship endpoints) are folded into a new `findings` section of the `context-pack` envelope, so the agent gets the authored "why" without a separate `tsift finding list` call. Injection is guarded: `draft` findings, stale findings (anchor advanced past the captured watermark), and findings with no captured watermark are excluded; the section is result-set-scoped (never a blanket store dump), capped at the preview-item budget, body-truncated, omitted when empty, and fails open to empty when `findings.db` is absent. Each preview carries a stable `handle`, `id`, `kind`, `title`, `about`, `anchor_kind`, optional `confidence`, and an `expand` command back to the full anchored set. New `collect_injectable_findings` helper in `commands/finding.rs`; compiled tests in `packages/tsift-cli/tests/finding.rs` cover injection, draft/stale exclusion, and result-set scoping. Spec updated in `specs/graph.md` § Hot-path injection (Phase 2). `search`/`explain` injection remains as `#trt1p2b`.
- **`#trt1p1`**: Findings Graph Layer Phase 1 (schema + anchored capture) shipped. New `tsift finding add` / `tsift finding list` commands store authored `finding`/`decision`/`note` nodes in a **dedicated `.tsift/findings.db`** (opened via `SqliteGraphStore`), independent of the rebuildable code projection at `.tsift/graph.db` so findings survive `graph-db refresh` and code reindex. `add` anchors `--about` to a `code_symbol` (body-span watermark) or `file` (file-content watermark) via a `concerns` edge, supports `--relates` (`relates_to` edge), `--confidence`, and `--status draft|trusted` (default `trusted`). `list` re-resolves anchor watermarks to flag staleness, hides stale findings by default, and surfaces them with `--include-stale`; a missing `findings.db` lists empty rather than erroring. Compiled tests in `packages/tsift-cli/tests/finding.rs` cover add/list roundtrip, all three node kinds, `relates_to`, staleness flagging, and the durability-across-projection-refresh invariant (`finding_survives_code_graph_projection_refresh`). Spec updated in `specs/graph.md`.
- **`#trt1`** (design-stage): documented the proposed Findings Graph Layer in `specs/graph.md` — authored `finding`/`decision`/`note` nodes anchored to stable symbol handles with watermark staleness, hot-path injection, graph-menu + md/html projection, and opt-in capture. Phase 1 now implemented (see `#trt1p1`); phases 2-4 remain proposed.
- **`#mdverify`**: Markdown semantic write coverage now locks the full guarded-write path. Compiled CLI tests cover Markdown block dry-run no-mutation reports with source-read follow-ups, invalid code-fence replacement refusal without mutation, temp-worktree `--verify` reports for Markdown source-read windows plus impact summaries, and failed `--verify-command --apply` gates that block real Markdown mutations.
- **`#mdblockedits`**: `edit-intents --apply` now supports Markdown block-level edits for `insert_list_item` and `rewrite_code_fence`. `insert_list_item` requires a unique indexed list-item target, preserves the target marker and indentation, and supports optional `position` (`before`/`after`). `rewrite_code_fence` requires a unique fenced-code target, replaces only the fence body, refuses replacement text that includes fence markers, and preserves the existing fence syntax/language. Apply diagnostics now include unsupported plan messages, so ambiguous Markdown targets surface the exact refusal reason before mutation. Tests cover block apply and ambiguous list/code-fence no-mutation refusal.
- **`#mdsectionedits`**: `edit-intents --apply` now supports Markdown section-level edits for `rename_heading`, `replace_section_body`, `insert_section`, and `move_section`. The executor resolves indexed heading targets, re-parses current Markdown buffers for each intent so sequential edits do not depend on stale byte offsets, validates output through tree-sitter Markdown, and writes through the existing atomic edit/rollback path without a formatter. `move_section` accepts `destination_symbol` plus optional `position` (`before`/`after`); `insert_section` can append or insert before/after a target heading. Tests cover dry-run support flags and batch apply for all four section intents.
- **`#mdsectionspans`**: Markdown indexing now gives read/write planning surfaces stable section and block spans. Heading symbols cover their full section body until the next same-or-higher heading, list items are indexed as `list_item` symbols, fenced code blocks expose code-body byte ranges, and `source-read`, `symbol-read`, and `edit-intents` span JSON now carries Markdown metadata for heading level, section path/handle, list depth, and fence language. Tests cover Markdown extraction, `source-read` heading/list/code-fence refs, and `edit-intents` target ranges for full-section heading spans.
- **`#mdastcontract`**: `edit-intents` now has a Markdown semantic edit language contract. `.md` and `.mdx` files resolve to canonical language id `markdown`, the contract binds to `graph::Lang::Markdown`, aliases `markdown`/`md`/`mdx`, declares no formatter, and recognizes `rename_heading`, `replace_section_body`, `insert_section`, `move_section`, `insert_list_item`, and `rewrite_code_fence`. The initial contract kept Markdown apply support fail-closed until executor phases could land behind the same validation surface. Tests cover the contract table, Markdown dry-run metadata/span resolution, and no-mutation refusal for unsupported Markdown intents.
- **language extension contract**: semantic edit executor language support is now declared through a `SemanticEditLanguageContract` table covering canonical language ids, aliases, file extensions, `graph::Lang` parser bindings, formatter suffixes/policies, script-family behavior, and supported intent kinds. The contract now drives language/file resolution, support flags, and formatter selection for the existing Rust, TypeScript/TSX, JavaScript/JSX, and Python executors. A contract unit test guards aliases, extensions, formatter policy, and the exact supported-intent set so future language additions fail fast when metadata is incomplete.
- **`#astverify`**: `edit-intents` now supports a `--verify` harness that applies supported semantic intents in a detached temporary git worktree before source mutation. The verifier reindexes the temp worktree before planning and after temp apply, runs bounded `source-read` windows plus an `impact` summary on the temp result, and can gate the real `--apply` path on a caller-provided `--verify-command`. Verification failures and failing verify commands fail closed before the real tree is mutated. Tests cover verify-only no-mutation behavior, verify+apply command gating, and failed-command rollback.
- **`#langexecutors`**: `edit-intents --apply` now has TypeScript/TSX, JavaScript/JSX, and Python executor adapters for the shared `rename_symbol`, `replace_function_body`, and `insert_import` contract. These executors resolve indexed tree-sitter symbol spans, validate source before mutation, fail closed for unsupported script-language call-site/structural intents, and reuse the atomic backup/rollback write path. Rust still formats through required `rustfmt`; script-language output is tree-sitter validated and uses a local `prettier`, `ruff format`, or `black --quiet` formatter when available. Tests cover TS body/import edits, JS rename edits, Python body/import edits, and no-write refusal for unsupported TypeScript call-site rewrites.
- **`#writecompound`**: `edit-intents --apply` now supports Rust structural edits for `add_method` and `move_declaration`. `add_method` targets indexed Rust `struct`/`enum` symbols, inserts into an existing inherent `impl`, or creates a new inherent `impl` next to the type. `move_declaration` moves an indexed declaration into an existing same-directory destination file, reports `destination_file`, inserts after the destination prelude, and normalizes the source module with `mod <destination>;` plus `use <destination>::<symbol>;`. Multi-file moves still run through formatter-before-write staging and the atomic backup/rollback path. Tests cover method insertion, cross-file declaration moves, and no-write refusal for unsupported same-file moves.
- **`#writerefs`**: `edit-intents --apply` now supports Rust call-site rewrites on indexed same-file refs. `rewrite_call_sites` replaces AST-validated Rust call expressions, and `update_call_signature` replaces the function signature plus indexed call sites when the intent supplies `call_replacement`. Plans expose `call_refs` for same-file indexed refs and fail closed for cross-file refs, missing call replacements, AST mismatches, stale hashes, and formatter failures before mutation. Fixture coverage exercises call-site apply, signature-and-call update apply, and no-write refusal for an incomplete signature update.
- **`#astspans`**: indexed symbols now persist AST-backed spans in addition to line ranges: declaration node kind, start/end byte offsets, and body start/end byte offsets. `source-read` symbol refs, `symbol-read` targets/child refs, and `edit-intents` target symbols now expose stable `span-*` handles with byte ranges, source-line projections, body ranges, and parent/child span refs when the index has the new columns; older read-only indexes fall back without span fields until refreshed. Tests cover span metadata on source-read, symbol-read, and semantic edit dry-run packets.
- **`#readast` / `#readrewrite` / `#writeintents` / `#writeexecutor`**: added the first explicit Read/Write replacement surfaces. `tsift symbol-read` resolves indexed symbols into bounded body packets with child symbol refs, summary refs, and expansion commands for source windows, `explain`, callers, and callees; `source-read` symbol refs now expand to `symbol-read` instead of jumping straight to `explain`. `tsift edit-intents` validates semantic AST edit intent batches (`rename_symbol`, `replace_function_body`, `insert_import`, `add_method`, `update_call_signature`, `move_declaration`, `rewrite_call_sites`) and emits dry-run plans with target ranges/content hashes, diff previews, and conflict detection. `edit-intents --apply` now applies supported Rust intents (`rename_symbol`, `replace_function_body`, `insert_import`) through formatter-before-write staging and the existing atomic backup/rollback edit path; unsupported kinds/languages, stale content hashes, and formatter failures fail closed before mutation. Tests cover symbol-read JSON output, source-read expansion routing, edit-intent dry-run planning, formatted Rust apply, stale-hash refusal, and formatter-failure no-write behavior.
- **`#token-action-coverage`**: added fixture-backed CLI coverage for `session-review --next-context` next-token actions beyond the existing `noop_closeout` case. The new fixtures exercise Codex token-count guardrails (`prompt_budget`, `cache_resend`) and agent-doc restart churn (`restart_loop`), asserting each actionable guardrail is emitted once in `next_token_actions` and collapsed out of `unresolved_failures` in bounded envelope output.
- **`#graph-neighborhood-budget`**: bounded graph-neighborhood previews now score before applying caps. `graph-db related` ranks incident/outgoing seed expansion edges, caps per-node edge scans and node discovery, and keeps semantic/source/caller/callee links ahead of low-signal high-degree noise before final truncation. `graph-db neighborhood` keeps stable node-id pagination unchanged but caps additive `ranked_neighbors` to a small score-sorted preview with a diagnostic. `tsift traverse <node> --limit N` now queue-expands scored incident neighbors first, so callers, callees, route handlers, backlog mentions, and semantic rows survive tight neighborhood budgets. Added regression coverage for score-before-cap semantic seed expansion, capped traversal neighborhood selection, and capped ranked-neighbor scoring.
- **`#agent-doc-domain-boundary`**: extracted the agent-doc/session observability domain out of the compatibility `tsift-session` crate into the new `tsift-agent-doc` workspace crate (`packages/tsift-agent-doc`). The moved domain owns `session_cost`, `session_digest`, `session_review`, agent-doc log parsing, restart-churn/guardrail derivation, and the fixture-backed digest/cost/review tests. Root `tsift` now re-exports those modules directly from `tsift-agent-doc`; `tsift-session` stays as a thin compatibility shim re-exporting the same modules and forwarding the `test-support` feature so existing `tsift-session` consumers and `tsift-sim-world` keep compiling. Pure crate-boundary split — no behavior change.
- **`#clippy-workspace-gate`**: closed the clippy coverage gap — flipped `make check`'s clippy leg from root-only `cargo clippy --all-targets` to **`cargo clippy --workspace --all-targets -- -D warnings`** (the lint analog of the `test:` → `--workspace` fix from `#split-sim-world`). Workspace clippy now gates every crate, not just root `tsift`. Cleared the remaining `tsift-cli` lint debt that the flip required: **deleted 8 orphaned `graph_db_*` wrapper fns** (`graph_db_neighborhood_depths`, `graph_db_page_handle_coverage_pct`, `graph_db_node_has_handle_coverage`, `graph_db_duplicate_name_precision`, `graph_db_has_community_signal`, `graph_db_has_semantic_signal`, `graph_db_source_handle_is_fresh`, `graph_db_edge_kind_rank_score`) — thin uncalled passthroughs to `tsift::resolution::*` left behind by an earlier refactor (zero call sites anywhere); **`#[cfg(test)]`-gated `traversal_relative_path_is_generated_artifact`** since its only four callers live in the lib test module; and added `#[allow(clippy::too_many_arguments)]` to `commands/graph.rs::cmd_explain` (11 args, a `#[cfg(test)]` budget-wrapper entry point). Updated SPEC release-verification note to `cargo clippy --workspace` / `cargo test --workspace`. Workspace clippy `-D warnings` clean across all crates; `make check` green; workspace tests unchanged at **993 passed across 40 suites** (deleted code was non-test; the gated fn still runs under test).
- **`#lint-cleanup-non-root-crates`** (post-extraction evaluation): while evaluating the digest/search/demote extractions, found that `make check`'s clippy leg runs `cargo clippy --all-targets` against the **root `tsift` package only** — the lint analog of the dead-test gap (`make check`'s `test:` was likewise root-only until `#split-sim-world`). `cargo clippy --workspace --all-targets -D warnings` surfaces pre-existing lint debt in several non-root crates that root-only clippy never checked. Cleaned the cheap/coupled ones: **tsift-tokensave** (dropped unused `VecDeque` + `GraphPagedSubgraph`/`GraphPropertyFilter`/`GraphQueryOptions`/`GraphSubgraph` imports); **tsift-libsql** (dropped unused `VecDeque`/`Duration`/`Graph*` imports, migrated the two deprecated `libsql::Database::open(...)` calls to `block_on(&rt, libsql::Builder::new_local(...).build())` matching the existing `open_remote` pattern, dropped two `unused_mut`, and scoped the test-only `GraphFreshness`/`GraphProjection`/`GraphProvenance` imports into the test module); **tsift-resolution** (three `useless vec!` → array literals in `scoring.rs` tests — newly visible because `#split-sim-world`'s `GraphFreshness` test-import fix made that test compile). The full `make check` clippy → `--workspace` flip is deferred (see backlog `#clippy-workspace-gate`) because `tsift-cli` still carries 9 `dead_code` `graph_db_*` helpers (flagged unused in both lib and lib-test) plus one `too_many_arguments` that need a delete-vs-gate decision first. Each cleaned crate: `cargo test` + targeted `cargo clippy --all-targets -D warnings` green; `make check` green; workspace tests unchanged at 993.
- **`#demote-root-tsift`**: with the sim-world / digest / search splits landed, drained the last real module out of root `tsift` and demoted it to a thin re-export shim. Extracted `status` (~2.07k LOC, `tsift status` + `tsift locks` backing) into a new `tsift-status` workspace crate (`packages/tsift-status`); root re-exports via `pub use tsift_status::status;`. status' `crate::` deps rewritten: `crate::config` → `tsift_index::config`, `crate::index::{IndexDb, WriterLockProbe, probe_writer_lock, writer_lock_path}` → `tsift_index::index::{...}`, `crate::init::{self, InstructionStatus}` → `tsift_index::init::{...}`, `crate::substrate::{ReadOnlyRecovery, rollback_journal_path, shared_memory_sidecar_path, wal_sidecar_path}` → `tsift_sqlite::{...}`, `crate::summarize::{SummaryDb, Summary}` → `tsift_summarize::summarize::{...}`. tsift-status deps: anyhow, serde, serde_json, rusqlite, tsift-index, tsift-sqlite, tsift-summarize (+ dev tempfile, fs4). **Root `tsift/src/` now contains only `lib.rs` plus five one-line re-export shims** (`graph` → tsift-graph, `lang` → tsift-graph::lang, `resolution` → tsift-resolution, `substrate` → tsift-sqlite, `libsql_backend` → tsift-libsql); lib.rs is entirely `pub use tsift_*` re-exports. Because root no longer has any code of its own, **pruned all external crate dependencies from root `tsift`** (anyhow, clap, flate2, fs4, ignore, rusqlite, serde, serde_json, toml, blake3, ureq, tagpath, tree-sitter, tempfile, optional libsql/tokio) and the now-unused `tsift-core` / `tsift-algorithms` direct deps — root keeps only the `tsift-*` path deps it re-exports plus optional `tsift-libsql`. Relocated the `examples/dump_ast.rs` tree-sitter AST-dump example into `packages/tsift-graph/examples/` (it uses `tsift_graph::lang::Lang` + `tree_sitter` and the lang-* features, all owned by tsift-graph). Verified default and `--features backend-libsql` builds plus `cargo install tsift`-path binary. Pure mechanical lift — no logic change. Workspace tests: **993 passed across 40 suites** (+2 suites; same total — status tests moved from root `tsift` into `tsift-status`); `make check` green.
- **`#split-tsift-search`**: extracted `sift` (local lexical search adapter), `impact` (change-impact analysis), and `tagpath_adapter` (tagpath family/member annotation) out of root `tsift/src/` into a new `tsift-search` workspace crate (`packages/tsift-search`). `summarize` was already extracted into `tsift-summarize` in a prior cycle, so this split covers the remaining three modules. Root `tsift` re-exports them via `pub use tsift_search::{impact, sift, tagpath_adapter};` so `tsift::sift::*` / `tsift::impact::*` / `tsift::tagpath_adapter::*` callers in `tsift-cli` keep resolving. `sift` and `tagpath_adapter` were already crate-internally self-contained; only `impact` carried `crate::` deps, rewritten to: `crate::{config, index, walk}` → `tsift_index::{config, index, walk}`, `crate::diff_digest::{self, DiffDigestOptions}` → `tsift_digest::diff_digest::{...}` (depends on the just-landed `#split-tsift-digest` crate), `crate::lang::Lang` → `tsift_graph::lang::Lang`, `crate::lint` → `tsift_quality::lint`, `crate::summarize` → `tsift_summarize::summarize`. `impact::is_import_line` gates per-language match arms on `#[cfg(feature = "lang-rust"|"lang-python"|"lang-typescript"|"lang-javascript")]`, so `tsift-search` defines the full `lang-*` feature set (default = all eight) forwarding to `tsift-graph`, and root `tsift` now forwards each `lang-*` to `tsift-search/lang-*` as well as `tsift-graph/lang-*`. No other root module consumed these three. Pure mechanical lift — no logic change. Workspace tests: **993 passed across 38 suites** (+2 suites; same total — sift/impact/tagpath_adapter tests moved from root `tsift`); `make check` green.
- **`#split-tsift-digest`**: extracted the four code-aware digest emitters — `diff_digest`, `log_digest`, `metric_digest`, `test_digest` (~3.7k LOC) — out of root `tsift/src/` into a new `tsift-digest` workspace crate (`packages/tsift-digest`). Root `tsift` re-exports them via `pub use tsift_digest::{diff_digest, log_digest, metric_digest, test_digest};`, so `tsift::diff_digest::*` / `tsift::test_digest::*` etc. paths used by `tsift-cli` (the digests command cluster) and any other consumer keep resolving with no caller changes. Rewrote the moved files' crate-internal imports to the now-external crates: `crate::graph` → `tsift_graph` (as `graph`), `crate::lang::Lang` → `tsift_graph::lang::Lang`, `crate::lint` → `tsift_quality::lint` (incl. inline `crate::lint::resolve_harness_root_or_canonical_path` in `log_digest`/`test_digest`), `crate::runtime_churn` → `tsift_quality::runtime_churn`, and `crate::summarize::{self, SummaryDb, Summary}` → `tsift_summarize::summarize::{...}`. `metric_digest` was already crate-internally self-contained. Deps: anyhow, serde, serde_json, tsift-graph (default/all-language features, matching the tsift-index precedent), tsift-quality, tsift-summarize; dev-dep tempfile. The digest modules are mutually independent (only `context_pack`, which lives in `tsift-cli`, composes them), so no inter-module deps crossed the boundary. Pure mechanical lift — no logic change. Workspace tests: **993 passed across 36 suites** (+2 suites; same total — the 16 digest tests moved from root `tsift` into `tsift-digest`); `make check` green.
- **`#split-sim-world`**: re-homed the orphaned SimWorld harness (`src/sim_world.rs`, ~390 LOC) into a new `tsift-sim-world` workspace crate (`packages/tsift-sim-world`) as a **test-harness crate** — `tsift` + `tsift-cli` are dev-dependencies, the deterministic seeded-corpus / named-edge-trace tests live in `tests/sim_world.rs`, and the crate's own `src/lib.rs` carries no production surface. This both completes the review item's extraction intent *and* fixes a latent dead-test bug: the harness had silently stopped compiling/running. Three root causes, all from earlier crate splits leaving the harness stranded: (1) `sim_world.rs` had no `mod sim_world` declaration anywhere, so it was never compiled; (2) it imported `crate::rewrite_command`, which now lives in `tsift-cli` as `pub(crate)` — unreachable from root `tsift`; (3) it called `session_digest::extract_prompt_targets_from_text_block`, which became `#[cfg(test)] pub(crate)` in `tsift-session` after the session split — invisible cross-crate. The literal "root `tsift` re-exports via `pub use`" framing in the review item was **impossible**: `sim_world.rs` imports `crate::{init, rewrite_command, session_digest, status}` where `status`/`rewrite_command` still live in root, so a `tsift -> tsift-sim-world -> tsift` lib re-export would be a cargo-rejected cycle. The test-harness (dev-dependency) shape avoids the cycle. Changes: promoted `tsift_cli::rewrite_command` `pub(crate)` -> `pub`; added a `test-support` feature to `tsift-session` that re-exposes `session_digest::extract_prompt_targets_from_text_block` as `pub` (`#[cfg(any(test, feature = "test-support"))]`), enabled by `tsift-sim-world`'s dev-dependency. **Also fixed the make-check coverage gap that hid the dead tests:** `make check`'s `test:` target ran bare `cargo test` (root `tsift` package only — 56 tests), so neither `tsift-cli`'s 479 tests, `tsift-sim-world`'s 3, nor `tsift-resolution`'s suite ever ran in CI/local check. Switched `test:` to `cargo test --workspace`. That surfaced one more stranded dead test — `tsift-resolution/src/scoring.rs` used `GraphFreshness` without importing it (a prior `#cli-cleanup-unused-imports` cycle had removed the import as "unused" precisely because the test never compiled); re-added `use tsift_core::GraphFreshness`. Full workspace: **993 passed across 34 suites**; `make check` green.
- **`#cli-cluster-digests` (Phase 2, final cluster)**: lifted the digests command cluster — `cmd_diff_digest`, `cmd_test_digest`, `cmd_log_digest`, `cmd_context_pack`, `cmd_metric_digest`, `cmd_session_digest`, `cmd_session_cost`, `cmd_session_review` (+ its `cmd_session_review_with_budget` engine) (9 functions, ~1347 LOC) — out of `packages/tsift-cli/src/lib.rs` into a new sibling `packages/tsift-cli/src/commands/digests.rs`. This completes the Phase 2 per-command-cluster split: every `cmd_*` body now lives under `commands/{index_search,graph,quality,summarize,infra,digests}.rs`. lib.rs re-imports the production entry points via `use commands::digests::{...}`; `cmd_session_review_with_budget` is the production engine (the thin `#[allow(dead_code)]` `cmd_session_review` wrapper stays in digests.rs and is not re-imported). Promoted 16 crate-private helper fns to `pub(crate)` (diff-digest label helpers, metric-digest gate/trend labels, `render_test_digest_from_input`/`render_log_digest_from_input`, the session-review/context-pack budget-report builders + human printers, `format_compact_count`) plus the `MetricDigestOptions` struct. Deliberately did **not** promote the JS identifiers `report`/`edges` that appear inside embedded HTML viz string literals (they matched a naive symbol scan but are not Rust items). tsift-cli tests: 479 passed across 7 suites; `make check` green.
- **`#cli-cluster-infra` (Phase 2)**: lifted the infra command cluster — `cmd_route`, `cmd_edit`, `cmd_convex_sync`, the seven `cmd_graph_db*` operator commands (`cmd_graph_db_status`, `cmd_graph_db_refresh`, `cmd_graph_db_doctor`, `cmd_graph_db_drift`, `cmd_graph_db_compact`, `cmd_graph_db_backend_eval`, `cmd_graph_db`), `cmd_status`, `cmd_locks`, `cmd_init`, `cmd_sql`, `cmd_rewrite` (15 functions, ~1371 LOC) — out of `packages/tsift-cli/src/lib.rs` into a new sibling `packages/tsift-cli/src/commands/infra.rs`, the largest and most entangled cluster. lib.rs re-imports the nine top-level cmd entry points via `use commands::infra::{...}` (`cmd_graph_db_*` sub-handlers are called internally from `cmd_graph_db`, so only `cmd_graph_db` is re-imported). Heaviest cluster by coupling: promoted ~59 crate-private helper fns, 15 private types (`ConvexHttpTransport`, `ConvexSyncOptions`, `EditBatch`/`EditResult`/`EditStatus`, the `GraphDbBackendEval*` family, `GraphDbCompactionReport`, `GraphDbDoctorReport`, `GraphDbDriftInput`, `GraphDbEvidenceInput`, `GraphDbExperimentalBackend`, `GraphDbRefreshSummary`), and 5 `GRAPH_*` consts to `pub(crate)` so the submodule can call them; the helpers and `GraphDbExperimentalBackend` business-logic impl stay in lib.rs (shared with the still-resident conflict-matrix/dispatch-trace/dependency-dag evidence commands). The compiler drove visibility promotion iteratively rather than manual tracing. `cargo fix` then stripped two now-unused lib.rs imports (the `tsift::substrate::{ConvexGraphStore, ConvexRowsGraphClient}` line and `GraphDbBackend`/`TraverseFormat` from the non-test cli import); `GraphDbBackend`+`TraverseFormat` were restored as a `#[cfg(test)]`-only import since the test module still references them. Pure mechanical lift — no logic change. tsift-cli tests: 479 passed across 7 suites; `make check` green.
- **`#cli-cluster-summarize` (Phase 2)**: lifted `cmd_summarize` (~281 LOC, single function but covers the full summarize surface — symbol lookup, file lookup, `--extract` with optional `--diff`, `--stats`, json/compact/human renderers) out of `packages/tsift-cli/src/lib.rs` into a new sibling `packages/tsift-cli/src/commands/summarize.rs` alongside `commands/{graph,index_search,quality}.rs`. lib.rs re-imports via `use commands::summarize::cmd_summarize`. Promoted 11 crate-private helpers to `pub(crate)`: `collect_source_files`, `emit_summary_stats_warnings`, `find_symbols_db_for_file`, `load_summarize_config`, `open_existing_summary_db_read_only`, `resolve_extract_base`, `resolve_extract_scope`, `summarize_diff_matches_scope`, `summarize_full_extract_deleted_summary_paths`, `summarize_relative_file_path`, `truncate_for_compact`. Pure mechanical lift — no logic change. `make check` green.
- **`#cli-cluster-quality` (Phase 2)**: lifted the quality command cluster — `cmd_audit_tagpath`, `cmd_audit`, `cmd_lint` (~412 LOC across three functions) — out of `packages/tsift-cli/src/lib.rs` into a new sibling `packages/tsift-cli/src/commands/quality.rs` alongside `commands/{graph,index_search}.rs`. Note: backlog originally listed `cmd_perf_gate`, but no such CLI command exists — `tsift::perf_gate` is a library API consumed by `cmd_graph_db_backend_eval`. lib.rs re-imports the three cmd entry points via `use commands::quality::{cmd_audit, cmd_audit_tagpath, cmd_lint}`. Promoted 2 crate-private helpers to `pub(crate)`: `tagpath_audit_policy_hints`, `tagpath_audit_supported_extensions`. Pure mechanical lift — no logic change. `make check` green.
- **`#cli-extract-tagpath-helpers` (Phase 1a-iii)**: extracted the tagpath consumer surface — `TagpathSearchOpts`, `TagpathAnnotationDiagnostic`, `CommunityMemberAmbiguityDiagnostic`, plus the five `annotate_*_with_tagpath` functions (`annotate_hits_with_tagpath`, `annotate_stored_symbols_with_tagpath`, `annotate_stored_edges_with_tagpath`, `annotate_communities_with_tagpath`, `annotate_path_nodes_with_tagpath`) — out of `packages/tsift-cli/src/lib.rs` (~307 LOC across structs+functions) into a new submodule at `packages/tsift-cli/src/output/tagpath.rs`. `output.rs` now declares `pub(crate) mod tagpath;`. lib.rs re-imports the eight names via `use output::tagpath::{...}` so the ~40 call sites in `commands/index_search.rs`, `commands/graph.rs`, and the rest of lib.rs keep compiling unchanged. Promoted 4 crate-private helpers to `pub(crate)` so the submodule can call them: `annotate_community_members_with_context`, `community_tagpath_cache_part_for_loaded`, `file_communities_from_callers`, `resolve_tagpath_handle_for_callee_edge`. Also dropped 5 unused imports introduced by Phase 2 cluster lifts (`PathBuf`/`Context`/`Serialize` in `commands/graph.rs`, `TraverseFormat` in lib.rs, plus the tagpath-file `Context`/`bail`). Pure mechanical lift — no logic change. `make check` green.
- **`#cli-cluster-graph` (Phase 2)**: lifted the graph command cluster — `cmd_graph`, `cmd_communities`, `cmd_traverse`, `cmd_path`, `cmd_explain`, `cmd_explain_with_budget` (~945 LOC across six functions) — out of `packages/tsift-cli/src/lib.rs` into a new sibling `packages/tsift-cli/src/commands/graph.rs` alongside the existing `commands/index_search.rs`. lib.rs re-imports the five production cmd entry points; `cmd_explain` is gated `#[cfg(test)]` because production dispatch goes through `cmd_explain_with_budget` (same default-budget-wrapper pattern as `cmd_search`). Promoted 18 crate-private helpers to `pub(crate)` so the child module can call them: traversal (`build_traversal_graph`, `traversal_report`, `traversal_report_html`, `verify_convex_projection_snapshot`), community/explain (`build_explain_budget_report`, `community_tagpath_cache_part`, `compact_members`, `detect_communities_cached`, `print_explain_budget_human`, `update_community_annotation_diagnostics`), edge rendering (`format_edge_groups`, `should_collapse_edge_groups`), path/symbol summaries (`symbol_path_summary`), index/tagpath (`open_index_db`, `query_tagpath_root`), and relativization (`relativize_edges`, `relativize_symbols`), plus the `shell_quote` helper. `CommunityDetectionReport` struct also promoted to `pub(crate)`. Pure mechanical lift — no logic change. tsift-cli tests still pass; `make check` green workspace-wide.
- **`#cli-cluster-index-search` (Phase 2)**: moved `cmd_index`, `cmd_search`, `cmd_search_with_budget`, and `cmd_search_worker` bodies (~800 LOC across four functions) out of `packages/tsift-cli/src/lib.rs` into a new sibling submodule at `packages/tsift-cli/src/commands/index_search.rs`. Introduced the `commands` parent module (`packages/tsift-cli/src/commands.rs`) so subsequent per-command clusters (graph, quality, summarize, infra, digests) can land alongside. Lib.rs now `mod commands;` and re-imports the four cmd entry points; `cmd_search` is gated `#[cfg(test)]` because it's only the test-facing default-budget wrapper. Promoted 30 crate-private helpers to `pub(crate)` so the child module can call them: index helpers (`run_index_update`, `relativize_index_summary`, `to_json_schema`), search-strategy/precheck helpers (`resolve_search_strategy`, `precheck_search_indexes`, `degraded_search_mode`, `emit_degraded_search_note`, `maybe_apply_search_*_test_hooks`), federated search (`federated_symbol_search`, `federated_exact_search`, `federated_sift_search`), relativization (`relativize`, `relativize_symbol_hits`, `relativize_json_paths`), search runners (`run_exact_search_with_timeout`, `run_search_with_timeout`, `run_sift_search`), budget/envelope formatting (`build_search_budget_report`, `build_search_budget_follow_up`, `print_search_budget_human`, `print_json_or_envelope`, `envelope_metric`, `inject_tagpath_stale_into_json`), and rendering (`should_collapse_search_hits`, `group_search_hits`, `compact_snippet`, `format_score`, `abbreviate_match_type`, `abbreviate_kind`). `DegradedSearchMode` also promoted to `pub(crate)`. Pure mechanical lift — no logic change. tsift-cli tests: 479 passed across 7 suites; `make check` green workspace-wide.
- **`#cli-cleanup-unused-imports`**: dropped the pre-existing clippy errors that kept `make check` red across `tsift-cli`, `tsift-resolution`, and `tsift-algorithms`. tsift-cli: removed unused `DEFAULT_BUDGET_BYTES`, `DEFAULT_BUDGET_ITEMS`, `DEFAULT_FOLLOW_UP_ITEMS` from the `output` import. tsift-resolution: removed unused `GraphFreshness` from `scoring.rs`. tsift-algorithms: `coupling.rs` swapped `.or_insert_with(HashSet::new)` → `.or_default()`; `dead_code.rs` collapsed an `if let` + nested `if`; `scc.rs` + `health.rs` added targeted `#[allow(clippy::too_many_arguments)]` to the inner Tarjan SCC recursive closures, and `health.rs::build_graph` got `#[allow(clippy::type_complexity)]` on its 4-tuple return. No behavior change. `make check` now exits 0 (clippy clean across the workspace; cargo test passes; opencode npm publish dry-run ok).
- **`#cli-extract-cli-types` (Phase 1b)**: extracted clap CLI parser types from `packages/tsift-cli/src/lib.rs` into a new `packages/tsift-cli/src/cli.rs` module — `Cli`, `Commands` (~770 LOC enum spanning all 40 subcommands), `GraphDbQuery` (~150 LOC subcommand enum), and the four `ValueEnum` output-format enums (`TraverseFormat`, `DispatchTraceFormat`, `SemanticRelatedKind`, `GraphDbBackend`). Pure mechanical lift with no logic change: field/variant visibility promoted to `pub` so dispatch sites in `lib.rs` (the `Cli::parse()` entry, ~130 `Commands::*` and `GraphDbQuery::*` arms, and 2 `Cli::parse_from` / `Cli::try_parse_from` test helpers) keep compiling after the move. `lib.rs` now declares `mod cli;` and imports the seven names with `use cli::{...}`; `clap::{Subcommand, ValueEnum}` are no longer needed at the lib.rs top level (only `Parser` remains, for `Cli::parse`). `GraphDbExperimentalBackend`, `GraphDbBackendPromotionGate`, and the backend-eval `impl` block stay in `lib.rs` because they carry business logic, not parser surface. tsift-cli tests: 298 unit + 129 integration passed.
- **`#cli-extract-output-envelope` (Phase 1a-ii)**: extracted `ToolEnvelope`, `ToolEnvelopeMetric`, `ToolEnvelopeSummary`, and `TranscriptArtifactRef` from `packages/tsift-cli/src/lib.rs` into the existing `packages/tsift-cli/src/output.rs` module. Pure struct relocation with no logic change; fields promoted to `pub` so envelope construction sites in `lib.rs` (~40 sites) keep compiling after the move. Workspace tests: 990 passed.
- **`#cli-extract-output` (Phase 1a-i)**: extracted `OutputFormat`, `ResponseBudget`, `ResponseBudgetPreset`, `adaptive_response_budget`, and `DEFAULT_BUDGET_ITEMS` / `DEFAULT_BUDGET_BYTES` / `DEFAULT_FOLLOW_UP_ITEMS` from `packages/tsift-cli/src/lib.rs` (~115 LOC) into a new `packages/tsift-cli/src/output.rs` module. lib.rs now declares `mod output;` and imports the names with `use output::{...}`. Phase 1a-i validates the per-module pattern at low risk; `ToolEnvelope` + `TranscriptArtifactRef` + `TagpathSearchOpts` + `TagpathAnnotationDiagnostic` + the `annotate_*_with_tagpath` family stay in lib.rs for the next sub-phase because they pull in cross-cutting types (`CommunityResult`, `CommunityMemberAmbiguityDiagnostic`) that are cleaner to move alongside the graph/community command cluster. Workspace tests: 990 passed.
- **`#split-tsift-summarize`**: extracted `summarize` (~1.8k LOC) from root `tsift` into a new `tsift-summarize` workspace crate (`packages/tsift-summarize`). This is the **Y** resolution to the digest↔search cycle — `summarize` becomes a shared foundation that `tsift-digest` and the future `tsift-search` both depend on, breaking the cycle without inverting either crate's natural dataflow. Future consumers (`tsift-mcp`, `tsift-agent`) can pull cached summaries without the rest of search. Root `tsift` re-exports via `pub use tsift_summarize::summarize;` so `tsift::summarize::*` paths used by `tsift-cli` and remaining root modules (`diff_digest`, `log_digest`, `test_digest`, `impact`, `status`) continue to resolve. Imports rewritten: `crate::index::IndexDb` → `tsift_index::index::IndexDb`, `crate::substrate::*` → `tsift_sqlite::*`. Deps: anyhow, blake3, fs4, rusqlite, serde, serde_json, ureq, tsift-index, tsift-sqlite. Workspace tests: 990 passed.
- **`#split-tsift-session`**: extracted `session_cost`, `session_digest`, and `session_review` (~7.1k LOC, largest single root pool) from root `tsift` into the new `tsift-session` workspace crate (`packages/tsift-session`). As a prereq, also moved `lint` into `tsift-quality` (it depends on `config` + `index::IndexDb`, both already in `tsift-index`); `tsift-quality` now depends on `tsift-index`, and `tsift-session` depends on `tsift-quality` (for `runtime_churn` + `lint::resolve_harness_root_or_canonical_path`). Root `tsift` re-exports `lint` from tsift-quality and the three session modules from tsift-session, so all `tsift::lint::*` / `tsift::session_cost::*` / `tsift::session_digest::*` / `tsift::session_review::*` paths used by `tsift-cli` (~20 lint sites) and the remaining root modules (`diff_digest`, `impact`, `log_digest`, `test_digest`, `status`) continue to resolve. Inline `crate::lint::*` / `crate::runtime_churn::*` paths inside the moved session files were rewritten to `tsift_quality::lint::*` / `tsift_quality::runtime_churn::*`. Workspace tests: 990 passed.
- **`#split-tsift-index`**: extracted `config`, `walk`, `init`, and `index` from root `tsift` into a new `tsift-index` workspace crate (`packages/tsift-index`). Root `tsift` re-exports the four modules via `pub use tsift_index::{config, index, init, walk};` so `tsift::config::*` / `tsift::index::*` / `tsift::init::*` / `tsift::walk::*` paths used by `tsift-cli` and the remaining root modules continue to resolve. Promoted `WriterLockProbe` and `probe_writer_lock` from `pub(crate)` to `pub` so `status` (still in root tsift) can keep importing them. Repointed two stale references: `index.rs` test imports moved from `crate::substrate::*` to `tsift_sqlite::*`, and `packages/tsift-cli/tests/perf_gate.rs` switched from `#[path = "../../../src/perf_gate.rs"]` to `use tsift_quality::perf_gate;`. Bumped tsift-index to `0.1.62` so `TSIFT_VERSION = env!("CARGO_PKG_VERSION")` stays aligned with the binary version. Workspace tests: 990 passed.
- **`#split-tsift-quality`**: extracted `audit`, `perf_gate`, `dci_benchmark`, and `runtime_churn` from root `tsift` into a new `tsift-quality` workspace crate (`packages/tsift-quality`). Root `tsift` re-exports the four modules via `pub use tsift_quality::{audit, dci_benchmark, perf_gate, runtime_churn};`, so `tsift::audit::*` / `tsift::runtime_churn::*` paths used by `tsift-cli` and other root modules continue to resolve. `lint` stays in root `tsift` for this pass because it depends on `config + index::IndexDb`; it joins `tsift-quality` after `tsift-index` lands. No public API surface change.
- **`#gopencode-install`**: added the `opencode-tsift` npm package for OpenCode registry installs. The package carries the same marker-owned `/tsift-*` command templates as `tsift init --opencode`, installs them into project `.opencode/commands/` on plugin load, exposes a direct `opencode-tsift` CLI installer, and refuses unmanaged command-file conflicts. Cargo tests now assert the npm package version and command files match the Rust init output, and `make check` runs the Node installer tests plus `npm publish --access public --dry-run`. The release workflow now dry-runs the npm package and can publish it on version tags when `TSIFT_ENABLE_NPM_PUBLISH=true` and `NPM_TOKEN` are configured.
- **`#gcachemiss`**: stabilize full-projection backend-eval cache reuse by hashing the semantic rows read from `.tsift/summaries.db` instead of the SQLite file metadata. Metadata-only summary-cache churn no longer shifts the traversal source watermark, while semantic summary row changes still invalidate the cache. The graph DB performance gate now states that full-projection samples become binding hop-cap/backend evidence only after a cold populate leg proves a cache-hit leg (`full_projection.cache.hit=1`). Evidence: `plans/gcachemiss-evidence.md`.
- **`#gback`**: lock the optional backend-adapter spike into `graph-db backend-eval` instead of adding an unproven graph database dependency. Reports now emit `performance_gate.backend_adapter_spike` for FalkorDB and Kuzu with the required real-adapter checks: provider-neutral projection writes/load, SQLite parity on every `GraphStore` operation, lock semantics, install portability, and faster-than-SQLite results across real, full-projection, high-degree, and deep-chain workloads. Conformance coverage asserts that read-only prototype evidence cannot satisfy the promotion gate. Evidence: `plans/gback-evidence.md`.
- **`#gfront`**: refreshed full-projection backend-eval evidence shows SQLite path/evidence reads are not the current graph bottleneck, so the SQLite traversal path stays on the existing indexed frontier implementation instead of taking an unmeasured rewrite. The performance gate now requires full-projection SQLite evidence-target, evidence, and 64/128/256/512-hop path duration metrics, and `tests/scan_plan.rs` locks chunked frontier probes to `idx_graph_edges_from_kind` so future high-hop work cannot regress into broad edge scans. Evidence: `plans/gfront-evidence.md`.
- **`#ghop`**: keep the user-facing graph path default at 64 hops until a dedicated hop-cap promotion gate passes. `perf_gate::evaluate_hop_cap_promotion` now requires repeated SQLite samples for `real`, `full_projection`, and `synthetic_deep_chain` workloads before any 128/256/512-hop tier can become the default; each candidate tier must stay within the latency regression budget against `path_max_hops.duration_micros` and return useful row counts, with deep-chain samples proving extra rows beyond the 64-hop median. `graph-db backend-eval` now emits the promotion contract in `performance_gate.hop_cap_promotion`. Tests cover fixture blocking, missing full-projection proof, latency regressions, and deep-chain row usefulness.

## 0.1.62

- **`#convexsnapshotmetascale` / `#gdbvacproof`**: `snapshotMeta` no longer iterates the full `nodes` and `edges` tables to compute `nodeCount` / `edgeCount`. The Convex examples now return cheap metadata (`indexes`, `pageSize`) plus an indexed `projectionHash` lookup for `projectionMetaId`; the Rust HTTP transport sends that id and, when the remote projection hash matches the local projection hash, treats freshness as current without walking every row page. Missing or mismatched hashes still fall back to the paginated row diff. Tests: `convex_sync_remote_snapshot_uses_projection_hash_shortcut_when_current` and `convex_sync_remote_snapshot_uses_paginated_transport_against_mock_backend`.
- **`#gdbvacproof` closed operationally**: self-hosted Convex apply against the agent-loop superproject graph completed at full scale with **19,344/19,344 chunk receipts ok** (358,439 node upserts, 608,739 edge upserts, default chunk size 50). The remote metadata hash matched the apply report hash (`fb0439ab06c8d08f615ade87b374adf118587943ad29d970650c0e2a0f982257`), so the guarded local prune ran with `--confirmed-convex-reconciled`: **1,240,258 tombstones pruned**, graph.db dropped **1,875,492,864 -> 1,016,492,032 bytes** (-859,000,832 bytes / ~819 MiB), freelist dropped **112,683 pages -> 0**, and post-prune `graph-db status` reports 0 tombstones / 0 freelist. Evidence: `plans/gdbvacproof-evidence.md`.
- **`graph-db compact` cache correctness**: pruning tombstones now refreshes the `graph_operator_stats` cache after VACUUM, so `graph-db status` and compact `counts_after` report the actual post-prune tombstone/file/freelist counts instead of stale pre-prune cached values. Covered in `sqlite_projection_refresh_tracks_versions_watermarks_and_tombstones`.

## 0.1.61

- **`#tpauditscope`**: lock in `tsift audit-tagpath --scope <name>` test coverage. The 0.1.59 implementation routes through `config::Config::resolve_submodule` + scope.source_root, but `audit_tagpath_reports_walker_diff` only exercised the workspace-root path. New `audit_tagpath_scope_reports_per_submodule_walker_diff` test in `tests/exit_code.rs` builds an `alpha` + `beta` workspace where `alpha/__pycache__/lib.rs` is tsift-indexed but tagpath-skipped (alpha has its own `.naming.toml` / `.naming/index.json`), and `beta` is fully covered. Asserts `--scope alpha` reports `__pycache__/lib.rs` in `tsift_only_files` and does NOT leak any beta files, then asserts `--scope beta` returns an empty diff in both directions. Locks in per-submodule scoping. Closes `#tpauditscope`. No source changes.

## 0.1.60

- **`#p6tsifullscoped`**: `tsift search --scope <name>` and inferred-scope search paths now annotate against the scope's `source_root` instead of the workspace root. Mirror of the federated bug closed in 0.1.57 (`#p6tsifullfederated`): when a submodule owns its own `.naming.toml` / `.naming/index.json` but the workspace root does not, scoped searches previously returned zero `tagpath_handle` because the adapter walked up from the workspace root and reported `Missing`. Single-line fix in `cmd_search_with_budget`: pass `&sift_path` (already populated with `scope.source_root` for scoped paths and the workspace root otherwise) into `annotate_hits_with_tagpath`. The federated path is unaffected because it carries its own per-scope diagnostic and skips this branch. Test: `search_scoped_json_annotates_handles_from_submodule_tagpath` in `tests/exit_code.rs` builds a single-submodule workspace where only the submodule has a tagpath project and asserts `tsift search --scope alpha --json scoped_helper` returns a `mem:` handle. Verified the test fails before the fix and passes after. Closes `#p6tsifullscoped`.

## 0.1.59

- **`#tpaudit`**: new `tsift audit-tagpath [--path .] [--scope <name>] [--json]` command reconciles the tsift symbol index against the tagpath `.naming/index.json` source set and reports files covered by one walker but not the other. Today silent recall loss happens when tagpath's `SKIP_DIRS` (e.g. `__pycache__/`, `vendor/`, `node_modules/`) or `[exclude]` / `extends` chain skip files that tsift still indexes — the tsift symbols in those files get no `tagpath_handle` even with a fresh index. The audit emits: `tsift_only_files` (tsift-indexed paths missing from tagpath), `tagpath_only_files` (tagpath paths missing from tsift), and a per-file symbol count breakdown so operators can see how many lookups are at risk. JSON shape: `{ project_root, scope, tagpath_state, tsift_file_count, tagpath_file_count, tsift_only_files, tagpath_only_files, tsift_only_symbol_count, tsift_only_files_with_symbols }`. When the tagpath index is stale, the audit loads the on-disk snapshot anyway, marks `tagpath_state: "stale"`, and injects the now-standard `tagpath_index_stale` / `tagpath_stale_reason` pair. Implementation: `cmd_audit_tagpath` resolves the tagpath root from `--scope` when given, opens `IndexDb` read-only, normalizes tsift's absolute paths to relative-to-root, and diffs against `adapter.index.sources`. Test: `audit_tagpath_reports_walker_diff` uses a `__pycache__/lib.rs` + `main.rs` fixture, runs `tsift index` then `tsift audit-tagpath --json`, and asserts the cached file shows up in `tsift_only_files` with its symbol count. Closes `#tpaudit`.

## 0.1.58

- **`#tpstdiag`**: surface the tagpath stale-index signal into the structured `tsift --envelope` / `--json` output. `tsift search`, `tsift path`, `tsift explain`, `tsift graph`, `tsift communities`, and the search/explain budget-mode reports now add a top-level `tagpath_index_stale: true` + `tagpath_stale_reason: <reason>` pair to the JSON response whenever any of the `annotate_*_with_tagpath` helpers reported `stale=true`. The existing stderr `tagpath_index_stale: …` log line is preserved unchanged. `--no-tagpath` suppresses both the stderr line and the new JSON fields. JSON consumers (tsift / agent-doc / external agents) can now decide to re-run with `--tagpath-strict` or trigger a rebuild from the structured response instead of having to scrape stderr. Implementation: new `inject_tagpath_stale_into_json` helper applied at every JSON emission site; `cmd_explain` folds the per-side and community diagnostics into a single combined `tagpath_stale` / `tagpath_stale_reason` pair; `cmd_graph` uses a `RefCell`-tracked diagnostic state across its caller / callee / combined emit paths; `cmd_communities` and `cmd_path` capture and inject directly. Test: `json_surfaces_tagpath_stale_diagnostic_when_index_is_stale` in `tests/exit_code.rs` builds a fresh tagpath index, mutates source to make it stale, and asserts the new fields appear on `tsift path`, `tsift communities`, `tsift graph`, `tsift explain`, and `tsift search` JSON responses, plus that `--no-tagpath` suppresses them. Closes `#tpstdiag`.

## 0.1.57

- **`#p6tsifullfederated`**: `tsift search --federated` now annotates each per-scope hit against that scope's own tagpath project (`scope.source_root`) instead of the workspace root, so federated searches over a workspace where each submodule has its own `.naming.toml` / `.naming/index.json` resolve `tagpath_handle` for every hit. Implementation: `federated_symbol_search` runs the annotation pass per scope inside the submodule loop and returns a merged `TagpathAnnotationDiagnostic`; `cmd_search_with_budget` skips the workspace-root annotation when the federated path supplied its own diagnostic. Test: `search_federated_json_annotates_handles_from_per_scope_tagpath_indexes` in `tests/exit_code.rs` builds an `alpha` + `beta` workspace with per-scope tagpath indexes and asserts both scopes return `mem:` handles. Unblocks federated agents that cite cross-submodule symbols by stable handle. Closes `#p6tsifullfederated`.
- **`#convexscopedeval` verdict**: `tsift convex-sync` defaults to **full-graph sync**; `--scope <name>` is the supported escape hatch, not the primary mode. Now that `#convexsnapshotscale` (v0.1.56) shipped cursor-paginated `snapshotMeta` / `snapshotNodesPage` / `snapshotEdgesPage` queries, one Convex deployment can hold the whole project graph. Scope-bounded sync stays valid for (a) reconciling one submodule independently mid-flight, (b) very-large projects where a single Convex deployment is not the operational target, (c) partial-failure recovery where only one scope's chunks need replay. Tooling on top of `convex-sync` should default to the full-graph form and surface `--scope` as a recovery option.
- **Default `--chunk-size` lowered from 100 to 50** to keep `upsertEdges` under the Convex isolate's 99 MiB carry-over budget on the demo schema. The previous default tripped the isolate carry-over limit on the agent-loop graph at chunk 100; chunk 50 cleared it on the same workload. Operators targeting a schema that has optimized its upsert mutations can still raise it back to 100 or higher. CLI help string updated with the rationale. Closes `#convexscopedeval`. Evidence: `plans/convexscopedeval-evidence.md`.

## 0.1.56

- `tsift graph`, `tsift explain`, `tsift path`, and `tsift communities` no longer drop the `tagpath_handle` for symbols whose name collides across files when the first-by-`(file, line)` definition lives outside the tagpath walk. The batch resolver (`resolve_tagpath_handles_for_names` in `src/main.rs`) now iterates every `symbol_info` row for a given name and keeps the first handle that resolves through the tagpath index, so a `main` symbol in `bin/foo/main.rs` plus `bin/bar/main.rs` reliably picks up the indexed copy instead of silently emitting no handle when the first row was excluded. Test: `communities_json_resolves_handle_through_name_collision` in `tests/exit_code.rs` uses a `__pycache__/main.rs` + `src/main.rs` fixture (tagpath skips `__pycache__/`, tsift indexes both). Closes `#commhx`.
- `tsift convex-sync --remote-snapshot` now fetches the Convex graph snapshot through a cursor-paginated transport (`snapshot_meta` + `snapshot_nodes_page` + `snapshot_edges_page`) instead of the single-shot `snapshot` query, so reconciliation works against tables larger than ~5k rows. The legacy single-shot `snapshot` operation hit Convex's 15s per-request syscall budget around that scale and blocked `--confirmed-convex-reconciled` for the full agent-loop graph (357k nodes / 605k edges). The new transport keeps the row-level diff semantics (still reports `missing_nodes` / `stale_edges` etc.) by concatenating pages locally. Closes `#convexsnapshotscale`; unblocks the Convex half of `#gdbvacproof`. Evidence: `plans/convexsnapshotscale-evidence.md`.
- Schema-side changes mirrored in both `examples/convex-graph/graph.ts` (snippet pack) and `examples/convex-graph-app/convex/graph.ts` (deployable app). Adds `snapshotMeta`, `snapshotNodesPage(cursor, limit)`, and `snapshotEdgesPage(cursor, limit)` queries plus matching `snapshot_meta` / `snapshot_nodes_page` / `snapshot_edges_page` routes in the HTTP action. Pages default to 500 rows (capped at 2000) and use the existing `by_external_id` / `by_edge_key` indexes for stable cursor ordering. The legacy `snapshot` query and `snapshot` HTTP operation are retained for small-table back-compat and as a fallback when the backend reports `unknown operation`; new deployments and tooling should not depend on them.
- Internal: `ConvexHttpTransport::fetch_snapshot` now tries the paginated path first and only falls back to legacy on operation-unknown errors. New `ConvexSnapshotMeta` and `ConvexSnapshotPage` deserialization helpers. Added `convex_sync_remote_snapshot_uses_paginated_transport_against_mock_backend` integration test that stands up a stdlib `TcpListener` mock backend, forces a small page size to exercise the cursor loop, and asserts `snapshot_meta` is called first plus no legacy `snapshot` fallback. The existing ignored live-acceptance test (`live_convex_graph_backend_acceptance_applies_and_matches_graph_db_queries`) and its `fetch_live_convex_snapshot` helper were rewritten to use the paginated ops so they exercise the same code path on real backends.
- `#cachelookupshift` investigation (no behavioral change): added regression-locking unit tests for `traversal_source_watermark` and `traversal_relative_path_is_generated_artifact` covering `.tsift/`, `target/`, and `.agent-doc/` prefix variants plus look-alike paths (`a__target/`, `tsift-extras/`), and a back-to-back stability test that asserts the watermark is identical on a quiescent root and only invalidates on real source mutation. Investigation verdict: no fix needed — the artifact filter is correct, no directory mtime enters the hash, and the agent-loop `preparation_cache_lookup` miss is caused by genuine concurrent edits to tracked markdown under `tasks/` and `src/session-share/tasks/`. On a quiet repo (e.g. `src/tsift` itself) the cache hits cleanly (~2 ms `disk_hit` vs ~250 ms recompute). Evidence: `plans/cachelookupshift-evidence.md`.

## 0.1.54

- `tsift communities` now annotates each Louvain community member with the stable tagpath `mem:` handle when a fresh tagpath index is present at the project root. Adds `--no-tagpath` and `--tagpath-strict` flags. Continues the `#p6tsifullcommunities` rollout — fourth of the five symbol-graph surfaces (search / path / explain / graph already shipped).
- **BREAKING CHANGE:** `graph::Community.members` is now `Vec<graph::CommunityMember>` instead of `Vec<String>`, where `CommunityMember { name, tagpath_handle? }` mirrors `graph::PathNode` from v0.1.51. The JSON shape changes accordingly: `tsift communities --json` now emits each member as `{name, tagpath_handle?}` instead of a bare string. `tsift explain --json` already emitted this shape for `community.members` in v0.1.52 via a JSON wrapping layer; that wrapping is now gone — the struct serializes natively. Consumers that read `community["members"][i]` as a string must switch to `community["members"][i]["name"]`. Human output is unchanged except that members with a resolved handle now print as `name  [mem:…]`.
- Internal: new `annotate_communities_with_tagpath` helper iterates every community and annotates each `CommunityMember` in-place; replaces the previous `resolve_community_member_handles` helper that synthesized parallel `Vec<Option<String>>` output. `cmd_explain` no longer wraps community JSON manually — it annotates `comm_result.communities` once and serializes the struct directly. `tests/exit_code.rs` extends communities coverage with explicit baseline / `--no-tagpath` / fresh-index cases.

## 0.1.53

- `tsift graph` now annotates caller and callee edges with the stable tagpath `mem:` handle when a fresh tagpath index is present at the project root. Adds `--no-tagpath` to skip the lookup and `--tagpath-strict` to fail closed on a stale index. Completes the call-graph triplet started by `tsift search` (`#p6tsi`), `tsift path` (`#p6tsifullpath`), and `tsift explain` (`#p6tsifullexplain`).
- All three `tsift graph` JSON output paths annotate consistently: the `--callers`-only response (`{callers, total, truncated}`), the `--callees`-only response (`{callees, total, truncated}`), and the combined `{callers, callees, callers_total, callees_total, …}` response that runs when neither flag is set. Each edge serializes the same `tagpath_handle: Option<String>` field already added to `StoredEdge` under v0.1.52.
- No new shared types required — `cmd_graph` reuses `annotate_stored_edges_with_tagpath` with `EdgeSide::Caller` / `EdgeSide::Callee` from v0.1.52. Stale-index diagnostics emit at most once per command invocation. `tests/exit_code.rs` extends graph coverage with explicit baseline / `--no-tagpath` / fresh-index cases plus a `--callers`-only annotation test.

## 0.1.52

- `tsift explain` now annotates definitions, callers, callees, and community members with the stable tagpath `mem:` handle when a fresh tagpath index is present at the project root. Adds `--no-tagpath` to skip the lookup and `--tagpath-strict` to fail closed on a stale index. Continues the `#p6tsifullexplain` rollout of the tagpath consumer surface that started with `tsift search` (`#p6tsi`) and `tsift path` (`#p6tsifullpath`).
- `StoredSymbol` and `StoredEdge` gain a `tagpath_handle: Option<String>` field (serde-skipped when `None`). For `StoredEdge` the handle names the row's primary symbol — caller for caller rows, callee for callee rows. The shared index layer never writes the field; only consumers (currently `cmd_explain`) populate it through `annotate_stored_symbols_with_tagpath`, `annotate_stored_edges_with_tagpath`, and `resolve_community_member_handles`.
- **Additive JSON change:** `tsift explain --json` previously emitted `community` as `graph::Community` (`{id, members: [string], modularity_contribution}`). Members are now objects `{name, tagpath_handle?}` so each member can carry a handle. The `graph::Community` struct itself is unchanged — the standalone `tsift communities` command keeps the flat string array shape until `#p6tsifullcommunities` lands.
- Internal: introduces `EdgeSide::{Caller, Callee}` and four new annotation helpers in `main.rs` (`annotate_stored_symbols_with_tagpath`, `annotate_stored_edges_with_tagpath`, `resolve_community_member_handles`, `resolve_tagpath_handles_for_names`), mirroring the existing `annotate_hits_with_tagpath` / `annotate_path_nodes_with_tagpath` helpers. `tests/exit_code.rs` extends explain coverage with explicit baseline / `--no-tagpath` / fresh-index cases that assert every definition, caller, callee, and community member carries a `mem:` handle when the index is fresh.

## 0.1.51

- `tsift path` now annotates each emitted node with the stable tagpath `mem:` handle when a fresh tagpath index is present at the project root. Adds `--no-tagpath` to skip the lookup and `--tagpath-strict` to fail closed on a stale index (parallels the `tsift search` consumer surface and the broader `p6tsifull` rollout).
- **BREAKING CHANGE:** `tsift path --json` now emits `path` as an array of `{name, tagpath_handle?}` objects instead of a flat array of strings. Consumers should read `.name` per node and may optionally read `.tagpath_handle` for citation. Human output (compact / default) is unchanged except that nodes with a resolved handle now print as `name  [mem:…]`.
- Internal: introduces `graph::PathNode` (the per-step entries of `PathResult.path`) and `annotate_path_nodes_with_tagpath`, mirroring the existing `annotate_hits_with_tagpath` helper. `tests/exit_code.rs` extends path coverage with explicit `--no-tagpath` and fresh-index annotation cases.

## 0.1.50

- Pin the `tagpath` dep to `^0.11.0`. The `[dependencies]` entry now carries an explicit `version = "^0.11.0"` alongside the `path = "../tagpath"` override, codifying the supported range against the local tagpath 0.11.x line. Supersedes the v0.1.47 entry's reference to tagpath 0.17.1 (the local path dep is back at 0.11.0). Cargo.lock now resolves to the local 0.11.0 path build; downstream crates.io consumers (once tsift publishes) will fail closed on incompatible tagpath releases instead of silently pulling whatever the path override happens to resolve to.

## 0.1.49

- `#gdbprephot`: `conflict_matrix_preparation.context_pack_diff` is the dominant remaining hotspot in the four-named set (~445 ms default / ~481 ms full-projection median on agent-loop, vs `session_review_compute` ~368/370 ms, `impact` ~1.5 ms). `diff_digest::compute` now honors a new `max_parsed_files: Option<usize>` option that bounds per-file tree-sitter parsing, `git show HEAD:path` snapshot loads, and summary-cache lookups to the first `N` files in canonical sort order; files beyond the budget become path-only `DiffDigestFile` entries with a `parse_deferred_by_budget` warning. `build_context_pack_report_with_profile` wires this to `budget.preview_items()` (5 by default) since the preview only takes that many files anyway. Three-sample medians on agent-loop drop `context_pack_diff` from 445 ms to 289 ms default (-35%) and from 481 ms to 290 ms full-projection (-40%); parent `conflict_matrix_preparation` drops from 1759 ms to 1727 ms default (-2%) and from 1856 ms to 1770 ms full-projection (-5%). Aggregate `symbols_touched`, `call_edges_added`, and `call_edges_removed` now reflect only the parsed subset for context-pack preview consumers; full-fidelity counts remain available via direct `tsift diff-digest` invocations which keep `max_parsed_files: None`. New `perf_gate::evaluate_preparation_hotspot` plus `CONTEXT_PACK_DIFF_BUDGET_MICROS = 350_000` lock the post-fix ceiling: callers MUST hand it freshly-acquired samples (no cached prior-run values) and the gate fail-closes below 3 samples to satisfy the "do not trust stale ownership" constraint. Five new unit tests (`diff_digest_max_parsed_files_skips_tree_sitter_beyond_budget`, `preparation_hotspot_*`) plus three new integration tests in `tests/perf_gate.rs` lock both the parse-budget contract and the gate verdict directions.

## 0.1.48

- `IndexDb::inspect_read_only` consults a thread-local `InspectScopeGuard` cache so a single trusted pipeline (e.g., `build_context_pack_report_with_profile` → `context_pack_status_reminders` → `status::check_status`) inspects the same `(root, .tsift/index.db)` exactly once instead of twice. Search and every other top-level call site runs outside any guard and gets identical fresh-per-call behavior — the regression test `search_timeout_reports_reindex_when_index_turns_stale_during_worker_run` still passes. `prepare_agent_doc_index_gate` invalidates the scope cache after a successful refresh so post-refresh status reflects the new DB. Three-sample medians on agent-loop show `status_index_gate` cold-leg drops from ~324 ms to ~53 ms (~271 ms reduction, over the 200 ms target for `#gdbgatecold`) and warm `context_pack_status_reminders` drops ~400 µs per call. Two new tests (`build_context_pack_reuses_inspect_within_scope`, `inspect_read_only_outside_scope_does_not_cache`) lock the scoped-cache contract.

## 0.1.47

- Adopt tagpath's `.naming/index.json` as a stable symbol-graph adapter (`#p6tsi`). New module `src/tagpath_adapter.rs` (`try_load`, `TagpathAdapter`, `LoadResult`, `HandleResolution`) wraps `tagpath::index` and is used by `tsift search` to annotate each `SymbolHit` with a stable `mem:<sha256[0..16]>` `tagpath_handle` when a fresh tagpath index is present at the project root. New search flags `--no-tagpath` (skip lookup) and `--tagpath-strict` (fail closed on a stale index). Stale indexes fall back to live extraction with a `tagpath_index_stale: true` stderr diagnostic. Existing users without `.naming/index.json` see no behavior change. Bumps the local `tagpath` path dep to 0.17.1 (with a slim `lang-rust,lang-python,lang-javascript,lang-typescript` feature set) and the workspace `tree-sitter` requirement to `^0.26`.

## 0.1.46

- `impact::compute` now exposes sub-phase timers under `conflict_matrix_preparation.impact.{context_resolution, diff_digest, test_path_scan, index_open, call_edge_impacts, route_handler_impacts, import_impacts, report_assembly}` and short-circuits the three iteration phases (`add_call_edge_impacts`, `add_route_handler_impacts`, `add_import_impacts`) when their inputs are empty. Three-sample medians on agent-loop show `conflict_matrix_preparation.impact` drops from 789 ms to 1.6 ms (-100%) on the typical backend-eval cold path (no staged changes), and the parent `conflict_matrix_preparation` drops from 2572 ms to 1935 ms (-25%). When staged changes are present the iteration phases run as before. The new sub-phases surface as `0us` on cache-hit reports with the existing source/document/staged-diff watermark detail, and the conformance suite asserts the sub-phases exist on cold runs.

## 0.1.45

- `status_index_gate` is decomposed into three sub-phases reported as `conflict_matrix_preparation.status_index_gate.{prepare_agent_doc_index_gate, context_pack_status_reminders, load_tag_ontology_preview_context}`. Three-sample medians on agent-loop confirm `prepare_agent_doc_index_gate` (422 ms, 62%) and `context_pack_status_reminders` (266 ms, 39%) split the cost; ontology loading is effectively free. New `prepare_agent_doc_index_gate_cached` wraps the gate behind an in-process `(root, path_hint, scope, packet_label)` cache so repeated invocations within the same process — daemon use cases, tests, traversal+context-pack pipelines — reuse the inspection result. Single-shot CLI flows do not benefit yet because each helper currently fires only once per `backend-eval` pipeline; the cold-path inspection cost stays owned by future work. Cache-hit reports surface the new sub-phases as `0us` with the source/document/staged-diff watermark detail. Conformance suite asserts the sub-phases exist on cold runs.

## 0.1.44

- `session_review` discovery now stat-walks Claude JSONL and Codex JSONL directories and only reads content for at most `MAX_RECENT_CANDIDATES_PER_SOURCE=64` newest files per source, and the per-file read is header-gated: a `BufReader` extracts the harness-specific `cwd` from the first 256 KB so files whose cwd does not match the target are skipped before any full read. Measured against agent-loop's ~2 GB / 2323-file Codex history and ~1.5 K Claude sessions, `conflict_matrix_preparation.session_review_compute.session_discovery` median drops from 3562 ms to 154 ms (-96%), `session_review_compute` parent drops from 3719 ms to 272 ms (-93%), and `conflict_matrix_preparation` overall drops from 5888 ms to 2148 ms (-64%), measured with `tsift graph-db --json backend-eval` three-sample medians on agent-loop using the new 0.1.43 sub-phase timers.

## 0.1.43

- `session_review_compute` now reports `target_context_build`, `session_discovery`, `session_digest_total`, `session_cost_total`, `session_aggregation`, and `report_assembly` sub-phases under `conflict_matrix_preparation.session_review_compute.<sub>` so the dominant preparation hotspot can be resolved at sub-phase granularity instead of as one 3.3–4.6 s opaque cost. Cache-hit reports also surface the same sub-phases as `0us` skipped with the source/document/staged-diff watermark guard, and the graph-db conformance suite asserts the sub-phases exist on cold runs and stay within 50 ms instrumentation slack of the parent phase.

## 0.1.42

- Conflict-matrix cache hits now report session-review, status/index gate, context-pack diff, exploration, graph orchestration, staged-diff, and impact phases as skipped 0us reuse guarded by source/document/staged-diff watermarks; backend-eval also requires real 128/256/512-hop metrics before any higher path cap can be considered, and all read-only prototype backends stay on hold until a native production adapter proves projection writes/load, parity, install, and lock behavior.
- Traversal source watermarks now exclude `.agent-doc` runtime markdown snapshots/baselines, preventing backend-eval full-projection and conflict-matrix cache keys from being invalidated by agent-doc closeout artifacts.
- `graph-db backend-eval` now has an opt-in `--full-projection` dataset, reports 128/256-hop path-tier probes alongside the 64-hop default and one-hop direct probes, and keeps FalkorDB on a hold decision until a production adapter beats SQLite across full-projection conflict-matrix/evidence/dispatch-trace/path/install/lock gates.
- `conflict-matrix` preparation now exposes split timings for cache lookup, session-review compute, status/index gate, context-pack diff, exploration materialization, graph orchestration, staged diff, and impact, plus source/document/staged-diff keyed `.tsift/conflict-matrix-cache` reuse for prepared context, staged-diff, impact, evidence, and target-scoped graph packets across CLI invocations.
- Normal `graph-db refresh`, `conflict-matrix`, and `dispatch-trace` now reuse the same source-watermark cached projection path as backend-eval when source inputs are unchanged, and conflict/trace preparation builds target-scoped graph snapshots instead of loading every graph node and edge.
- GraphStore now exposes cheap count and sample-edge probes; SQLite backs them with `COUNT(*)` / indexed `LIMIT 1` queries so backend-eval status and `path_max_hops` selection avoid full row materialization before timing the measured operation.
- Graph refresh now streams materialized node-property rows into the staged SQLite projection while node rows are inserted, and context-pack exploration uses one batched SQLite projection transaction for `source_handle` / `worker_context` rows instead of per-row autocommit writes.
- SQLite graph DB schema v3 now maintains `graph_node_properties` rows so `graph-db kind` and `neighborhood --property KEY=VALUE` use an indexed materialized property table instead of JSON extraction scans; refresh/status/doctor expose compaction proof, and `graph-db compact` adds a guarded WAL checkpoint/VACUUM path with explicit Convex-reconciliation confirmation before tombstone pruning.
- `graph-db evidence` now batches reachable worker-context, source-handle, worker-result, and semantic row expansion through one SQLite recursive CTE per target, preserving max-hop/limit ordering while avoiding per-family path walks.
- `conflict-matrix`, `dispatch-trace`, and `graph-db backend-eval` now reuse one prepared graph orchestration bundle per target set, including evidence packets, source-handle/worker-result/semantic expansion, graph snapshots, and dispatch-trace inputs, instead of repeatedly rewalking the same rows; repeated CLI calls can load the bundle from `.tsift/conflict-matrix-cache` when source/document/staged-diff and graph-freshness watermarks match.
- `graph-db backend-eval` now measures real, synthetic high-degree, and synthetic deep-chain datasets, emits metric-digest-ready raw and per-1k-graph-row normalized metrics plus replay/repeated-sample commands, and includes `fixtures/graph-db-performance-history.json` for repeatable performance-history comparisons.
- Added `tsift dependency-dag --path <session> [target...] --json` with a `dependency-dag-v1` contract for agent-doc backlog nodes, explicit dependency text, shared file/symbol/test/config and semantic overlap edges, worker-result follow-up edges, deterministic topological batches, cycle diagnostics, replay commands, and repair commands.
- `conflict-matrix` candidates and `worker_prompt_packets` now expose `previously_completed`; completed worker_result evidence downgrades missing source ownership to an informational warning instead of `per_target_fail_closed`, preventing completed agent-doc queue items from being reactivated only to rediscover ownership.
- Graph orchestration JSON surfaces now publish explicit contract versions and replay metadata: `graph-db evidence` emits `packet_id`, `projection_hash`, explicit worker/semantic result arrays, `replay_commands`, and `repair_commands`; `conflict-matrix`, `worker_prompt_packets`, `context-pack graph_orchestration`, `session-review --next-context`, and `dispatch-trace` carry matching contract markers for agent-doc consumers.
- Completed/blocked agent-doc worker responses now materialize as `worker_result` graph rows linked to backlog/job/source handles with status, touched files, expected tests, and follow-up ids, and `conflict-matrix` summarizes them as worker feedback with repeated-blockage warnings that do not weaken hard conflict gates.
- Added `tsift dispatch-trace --format json|html` for compact graph-backed operator review views linking backlog, job_packet, worker_result, source_handle, semantic rows, evidence packet ids, worker feedback, and worker_prompt_packets.
- Semantic dispatch ranking now includes fixture-covered score explanations while keeping file/symbol/test/config overlap as the hard fail-closed gate.
- Added `fixtures/graph-db-operator-examples/graph-orchestration-contracts.json` plus end-to-end refresh/status/doctor/evidence/stale-Convex-drift/convex-sync/conflict-matrix/context-pack/session-review operator commands for graph-backed dispatch.
- `conflict-matrix` now emits first-class `worker_prompt_packets` with owned files/symbols, read-only context, forbidden files, expected tests, expansion commands, and token budgets; target-specific source ownership prevents unrelated workers from inheriting every visible source window.
- Graph orchestration observability now carries projection freshness, evidence packet ids, conflict-matrix decisions, ownership block labels, and follow-up graph commands through `conflict-matrix`, `context-pack`, and `session-review --next-context`.
- `graph-db evidence` now includes reachable semantic concept/entity rows, and `conflict-matrix` uses those semantic rows as a ranking signal without overriding file/symbol/test/config conflict gates.
- Release verification now runs `cargo publish --locked --dry-run`, and the release docs/tests lock the `TSIFT_ENABLE_CRATES_PUBLISH` variable plus `CARGO_REGISTRY_TOKEN` secret contract for tagged crates.io publishes.
- Added an opt-in live Convex graph backend acceptance harness that applies a temporary projection to a dedicated deployment, pulls the remote snapshot, and runs graph-db node/kind/neighborhood/path parity against SQLite.
- Added `tsift graph-db doctor` for read-only SQLite `graph.db` and Convex snapshot diagnostics, including stale projection metadata, schema drift, orphan edges, duplicate ids, missing Convex index metadata, repair commands, and fail-closed exit codes.
- `tsift graph-db kind` and `graph-db neighborhood` now support deterministic node-id cursor pagination, repeatable `--property KEY=VALUE` node filters, page diagnostics, and backend parity coverage across SQLite and Convex snapshot stores.
- Added `examples/convex-graph`, a reusable Convex app-side schema/mutation/HTTP-action package for `tsift convex-sync --remote-snapshot --apply`, plus a local HTTP smoke test proving apply chunks round-trip through the documented transport shape.
- Agent-doc queue entries now materialize as `job_packet` graph nodes, and `context-pack` exploration packets now include bounded `worker_context` nodes linked to source handles so worker handoffs preserve prompt scope in the graph substrate.
- Added `fixtures/graph-db-operator-examples` with SQLite graph-db commands, Convex sync/apply examples, a stale snapshot fixture, and handle-reuse guidance for `traverse` / `context-pack`.
- `tsift traverse` and `context-pack` now materialize provider-neutral graph rows into `.tsift/graph.db` before report generation, including projection metadata/freshness, source-handle nodes, and Convex snapshot fail-closed validation; `tsift convex-sync` emits dry-run Convex `nodes`/`edges` upsert, tombstone, chunk, index, and freshness diagnostics.
- Added `tsift traverse`, a Graphify-style traversal surface that exports JSON/HTML graph slices with stable `gfil-*`, `gsym-*`, `gses-*`, and `gbak-*` handles for files, symbols, agent-doc sessions, and backlog items, plus neighborhood, shortest-path, and next-node recommendation reports for bug-fix navigation.
- `tsift status` now emits structured stale-index reminders, and `context-pack` carries the same reminders forward so agent handoff packs still show the reindex command and missing-summary follow-up when the repo index is stale.
- `log-digest` no longer reports clean `quit_after_eof` / user Ctrl-D exits as restart-churn warning signals; those exits remain summarized in runtime churn context while actual fresh restarts, timeouts, and Ctrl-D restart loops continue to warn.
- `session-review --next-context` now carries aggregate guardrails forward as `guardrail:<kind>` unresolved-failure action rows, so restart-loop, prompt-budget, cached-resend, and no-op closeout warnings remain visible in resumable handoff context even when no command failure was extracted.
- `log-digest` now classifies agent-doc runtime failures, restart churn, timeouts, and closeout churn as warning/error signals, so agent-doc logs no longer report `signal_groups: 0` while `session-digest` sees runtime failures and churn.
- `session-cost` no longer emits `restart_loop` guardrails from `max_restart_count` alone; restart-loop warnings now require actual restart-churn families such as fresh restarts, auto-trigger timeouts, or ctrl-d restart loops, with max restart count kept as contextual detail.
- Codex JSONL `session-digest` file-reference extraction now rejects shell redirection fragments and slash-separated conversational labels such as `agent-doc/tsift`, `digest/session`, `progress/CI-status`, and `version/preflight` unless they resolve to real files or carry recognized file names/extensions.
- `tsift rewrite` now leaves file-listing commands such as `rg --files`, `rg --type-list`, and `find ...` on the no-rewrite passthrough path so listing roots and predicates are not misread as exact search patterns.
- `session-digest` and `session-review` now filter assistant progress and assessment prose about failure-classification false positives, unresolved failure groups, and red/check CI-status commentary instead of reporting those meta lines as failures.
- `token-savings` fixtures now support source-read rewrite rows with required line-anchor validation, and the real-session fixture covers full-file `cat`/`bat` plus oversized `sed`/`head`/`tail` reads under a fail-under threshold.
- `session-cost` now reports repeated source-file read diagnostics for Claude/Codex transcripts, grouping native `Read` and common shell reads by path/range with duplicate-token estimates and concrete `tsift source-read` / `tsift summarize --file` follow-ups; `session-review` aggregates the same diagnostics across matched sessions.
- `log-digest` and `session-digest` now filter agent-doc runtime path fields that normalize to empty display paths or existing directories, preventing project-root `cwd_resolved` events from polluting file anchors and next-context file lists.
- Added deterministic lock-contention regressions for direct `index`, `search`, and `status` paths when SQLite WAL/SHM sidecars are live without a tsift-owned `index.lock`, preserving WAL-aware snapshot fallback and recovery guidance instead of raw lock errors.
- `session-cost` now prefers Codex `last_token_usage` records when cumulative `total_token_usage` streams interleave in one rollout, while still skipping duplicate cumulative snapshots and preserving the cumulative-delta fallback for older transcripts; `session-review` inherits the corrected totals and largest-turn outliers.
- `session-review` now aggregates token, command, failure, guardrail, and loop-cluster totals over the same bounded newest matched session rows it emits, and reports the newest matched session in a separate `latest_session_cost` scope so cached multi-session totals cannot be mistaken for active-session cost.
- `session-digest` and `session-review` failure rows now carry parsed command/session anchors, filter active prompt directives and source snippets out of failure extraction, and preserve real assertion/panic evidence plus named command exits such as `cargo test exited with code 1`.
- `tsift search` now delegates free-text query normalization to the `tagpath` v0.6.2 query API, while search/explain/session-review/context-pack preview refs derive canonical `tag_alias` values from the shared `tagpath` family API instead of local parser helpers.
- `context-pack` now loads tagpath ontology docs from `.naming/tags/*.md` and attaches compact ontology references to visible symbol refs, summary refs, and the top-level handoff payload so stable tag docs can be referenced by handle/path without repeating ontology prose.
- Regression coverage now locks ontology refs in both preview-builder unit tests and the compiled `context-pack --json` integration path.
- Budgeted `session-review --next-context` now keeps follow-up digest commands on an independent 4-command floor and preserves them verbatim, so small previews do not hide or corrupt the resume commands measured by the real-session token-savings fixture.
- Added a tsift-local deterministic SimWorld for session prompt extraction, rewrite routing, and status recommendation edge coverage. The fast corpus runs in normal `cargo test`; the wider ignored corpus runs in GitHub Actions through `make ci-full`.
- The generated Code Navigation guidance now tells agents to run local `make check`, then inspect the latest GitHub Actions CI run with `gh run list --workflow CI --limit 1` and fix red CI before calling work complete.
- `tsift log-digest` now treats structured agent-doc runtime fields as anchors: `file=...` and `path=...` become file refs without requiring line numbers, while timestamped event names plus `event=...`, `pane=...`, and `session=...` become structured symbol refs.
- Added a compiled-CLI regression proving `tsift status --fix --json` refreshes stale Code Navigation instructions from the prior binary version and reports the upgraded instructions as current.

## 0.1.42

- Added `tsift status --fix`, which applies safe local status recommendations before reporting: refreshes stale/missing root indexes, refreshes existing workspace scoped indexes when stale, and updates stale/missing Code Navigation instructions through the existing `tsift init` path.
- The injected Code Navigation instructions now tell agents to run `tsift status --fix` before relying on stale/missing tsift results when writes are allowed, or ask the user to run the printed `run:` command when writes are not allowed.
- Regression coverage now locks `status --fix` in both the in-process status command and the compiled CLI JSON path.

## 0.1.41

- Added `--budget <small|normal|deep|auto>` to the agent-facing preview surfaces for `search`, `explain`, `session-review`, and `context-pack`.
- `tsift --envelope` now applies the adaptive budget by default when callers do not pass explicit caps, with `auto` reading `TSIFT_CONTEXT_WINDOW`, `CODEX_CONTEXT_WINDOW`, or `CLAUDE_CONTEXT_WINDOW` to select small/normal/deep defaults.
- `tsift rewrite` now emits `tsift --envelope search ... --exact --budget normal` for `rg` and recursive `grep` rewrites, keeping hook output compact while avoiding hard-coded numeric caps in the command surface.

## 0.1.40

- `tsift --envelope __digest-runner ...` now probes `rtk rewrite` when RTK is installed, executes supported generic command families through RTK's compact filters, and records the chosen filter under `report.filter` while preserving the original command, exit code, digest payload, and artifact-backed transcript.
- The digest-runner envelope summary now includes a `filter` metric, so harnesses can see whether a build/test/log surface was compressed by RTK or by tsift's built-in digest path alone.
- Regression coverage now locks the RTK delegation path with a fake `rtk` binary, including envelope metadata and persisted filtered artifact content.

## 0.1.39

- `tsift rewrite` now makes token-saving agent surfaces automatic: `rg` / recursive `grep` rewrites produce `tsift --envelope search ... --max-items 5 --max-bytes 160`, and cargo/pytest/build rewrites produce artifact-backed `tsift --envelope __digest-runner ...` commands by default.
- `tsift init` now injects envelope-first Code Navigation guidance for `search`, `explain`, `session-review`, `context-pack`, and digest-runner test/build artifacts, so Codex and other non-Claude harnesses get the same bounded workflow through `tsift rewrite --run '<command>'`.
- Regression coverage now locks the default rewrite shapes plus end-to-end `rewrite --run` envelope execution without requiring callers to pass a global `--envelope` flag.
- `tsift rewrite` now forwards and deduplicates global structured-output flags into rewritten tsift commands, so callers can layer `--pretty`, `--terse`, or `--schema` onto the default summary-first `digest-runner` envelope.
- Regression coverage now locks the forwarded rewrite shape for `cargo install` plus end-to-end `rewrite --run` envelope execution for real `cargo test` and `cargo build` commands on a temp crate.
- `tsift --envelope __digest-runner ... --json` now returns a summary-first command/test-run envelope with command metadata, exit status, the existing bounded `test-digest` or `log-digest` payload under `report.digest`, and a persisted transcript artifact reference under `report.artifact`.
- Captured runner/build output is now written to `.tsift/artifacts/` with a stable handle plus a concrete replay command (`expand`) so green runs can stay terse in context while still offering an opt-in path back to the bounded digest.
- `tsift rewrite --run` now disables the default timeout when it is executing an already-tsift `search` command that did not specify `--timeout`, so capped exact-search passthroughs do not fail spuriously on broader scans.
- Regression coverage now exercises the new digest-runner envelope end to end, including persisted artifact creation for a passing test run.
- Added a global `tsift --envelope` wrapper for the bounded agent-facing `search`, `explain`, `session-review`, and `context-pack` JSON surfaces. The envelope carries a terse cross-command `tool`/`view`/`summary`/`follow_up` header while preserving the existing command-specific payload under `report`.
- Preview and handoff commands now expose one consistent machine-readable summary layer plus concrete follow-up commands, so MCP or CLI clients can render terse summaries and trigger narrower expansions without depending on prose formatting.
- Regression coverage now locks the new flag in CLI parsing tests and exercises the wrapped `context-pack` JSON output end-to-end.
- Added `tsift context-pack`, a single agent-facing handoff command that composes `session-review --next-context`, `diff-digest`, and optional `test-digest` / `log-digest` inputs into one bounded payload with resume commands.
- `context-pack` is bounded by default and accepts `--max-items` / `--max-bytes` so callers can keep resumable context packs stable under token pressure without replaying raw transcripts, diffs, or verbose logs.
- Regression coverage now locks the new command surface in CLI parsing, preview-builder unit tests, and a compiled end-to-end integration test that exercises the composed JSON payload.

## 0.1.38

- `tsift search --autoindex` now degrades instead of failing when a live tsift `index.lock` holder is already refreshing the target index: stale indexes continue through a read-only search path, and missing indexes fall back to exact live-file search until the writer finishes.
- The degraded success path emits one concise retry hint on stderr so callers know why symbol hits may lag or why exact search was used, without requiring a separate `tsift locks` run.
- Regression coverage now locks both the in-process and compiled CLI behavior for stale-index read-only fallback plus missing-index exact fallback under a live writer lock, while keeping rollback-journal lock failures fail-closed.

## 0.1.37

- `tsift rewrite` now supports `--run`, which executes the rewritten digest-first tsift command directly instead of only printing it for Claude hook integration.
- `rewrite --run` preserves the rewritten command's exit status and applies tsift-owned output caps for verbose human-readable `search`, `explain`, `graph`, `communities`, and `index` output, so Codex and other harnesses can stay bounded without depending on Claude `PreToolUse` hooks or RTK.
- Updated the injected Code Navigation guidance, spec, and harness-facing docs to point non-Claude harnesses at `tsift rewrite --run '<command>'` as the manual bounded fallback.

## 0.1.36

- `tsift session-cost` now emits bounded `loop_clusters` summaries for repeated prompt bodies, repeated command bundles, and repeated closeout churn across Claude JSONL, Codex JSONL, and `agent-doc` runtime logs.
- `tsift session-review` now aggregates those loop clusters across matched sessions, so repeated verification bundles and no-op closeout churn become explicit review signals instead of hiding inside broad command/runtime totals.
- Regression coverage now locks the new loop-cluster surface in direct/unit tests and compiled CLI integration tests for both `session-cost` and `session-review`.

## 0.1.35

- `tsift session-review` now learns historical document path aliases plus prior `session=` aliases from the matching `agent-doc` runtime log before it scans Claude/Codex transcripts, so renamed task files and migrated session ids still collapse into one comparable review.
- File-target session matching no longer relies on filename-only aliases or arbitrary raw transcript substrings. Claude/Codex candidates now match only against structured user/tool-input snippets, which prevents unrelated hook output or command stdout from pulling in the wrong session history.
- Claude/Codex transcript parsing for `session-review` now skips malformed JSONL lines instead of failing the whole review, and Claude non-conversation attachment records are ignored without noisy warnings so cross-harness results stay comparable.

## 0.1.34

- `tsift session-review` now includes a bounded `next_context` payload in its JSON report and supports `--next-context` to emit only the resumable handoff pack for a document or repo target.
- The new next-context pack carries only active prompt targets, the latest verification closeout state, touched files/symbols, unresolved failure hotspots, and the next digest commands to use instead of replaying raw session/transcript/log context.
- Regression coverage now locks the new next-context surface in direct/unit tests, CLI parsing tests, and compiled CLI integration tests for both the full JSON review and the dedicated `--next-context` output.

## 0.1.33

- Added `tsift session-review`, a cross-harness aggregate review for a document or repo path. It auto-discovers related Claude JSONL, Codex JSONL, and `agent-doc` runtime logs, then emits one bounded combined digest + cost report instead of requiring manual per-log review.
- `session-review` reuses the existing `session-digest` and `session-cost` parsers, aggregates their bounded signals into one report, and matches document sessions by cwd/path plus `agent_doc_session` log aliases when available.
- File-target `session-review` matching now fails closed on shared-workspace cwd hits: Claude/Codex transcripts must also mention the target document path or `agent_doc_session` before they count as a matched session, while directory targets still use cwd matching.
- Regression coverage now locks the new command in direct/unit discovery tests, CLI parsing tests, and a compiled CLI integration test that exercises Claude/Codex/agent-doc auto-discovery through `HOME`.
- `tsift session-cost` and `tsift session-digest` now derive bounded restart-churn families from `agent-doc` runtime logs, so `fresh_restart`, `auto_trigger_timeout`, ctrl-d restart loops, and quit-after-eof cycles are summarized directly instead of being buried in raw event counters.
- Regression coverage now locks the new restart-churn summaries in both direct/unit tests and compiled CLI stdin tests for `session-cost` and `session-digest`.
- `tsift init` now injects owning-root guidance into the Code Navigation section so harnesses switch to the relevant repo or submodule root before tsift/build/test work instead of accidentally carrying the superproject instruction surface into submodule tasks.
- The injected Code Navigation section now also steers Claude/Codex toward `session-digest`, `session-review`, `diff-digest`, `test-digest`, and `log-digest` instead of raw transcript replays, `git diff/show/log` patch dumps, or verbose build/test output reads.
- Harness-oriented digests (`session-digest`, `log-digest`, `test-digest`) now prefer the nearest owning git root over the outer workspace `.gitmodules` root, so transcript reads and digest enrichment stay scoped to `src/tsift` when the source file lives there.
- `tsift rewrite` now anchors long transcript/log reads to that owning repo or submodule root before routing them into `session-digest`, and regression coverage now locks the new root-selection behavior in both direct/unit and compiled CLI rewrite tests.
- `tsift session-digest` now supports Codex JSONL and `agent-doc` runtime `.log` inputs in addition to markdown session docs and Claude JSONL, so bounded session evidence no longer depends on replaying raw harness transcripts or restart logs.
- `tsift rewrite` now recognizes long Codex JSONL reads and `agent-doc` runtime log reads and routes them to `tsift session-digest` instead of spilling raw session/log content into agent context.
- Regression coverage now locks the new session-digest parser paths and rewrite detection in both direct/unit tests and compiled CLI integration tests.
- Added `tsift session-cost`, a bounded token/runtime-cost digest for Claude JSONL, Codex JSONL, and `agent-doc` runtime logs. It reports prompt totals, cached-input ratios, output totals, largest turn outliers, and restart-churn counters without replaying the raw session.
- `session-cost` normalizes Claude cache-read/cache-create usage and Codex cumulative `token_count` events into one report, dedupes repeated Claude assistant message ids, and skips duplicate Codex cumulative snapshots so token totals stay stable.
- Regression coverage now locks the direct helper logic, CLI parsing, and compiled CLI stdin path for the new command.
- `tsift search` human-readable output now collapses repeated high-hit file matches into grouped file rows with hit counts before representative snippets, so broad exact/literal lookups stay usable without depending on RTK-only truncation.
- `tsift explain` now applies the same file-level grouping idea to dense caller/callee sets in its default human output, while leaving JSON and tabular outputs unchanged.
- Regression coverage now locks the grouped search/explain rendering in both direct/unit tests and compiled CLI integration tests.

## 0.1.32

- `tsift rewrite` now auto-routes long transcript reads for recognized agent-doc markdown sessions and Claude JSONL handoffs into `tsift session-digest` instead of spilling raw session history into agent context.
- The new transcript-read rewrite coverage is intentionally narrow: it only intercepts `cat`, `bat`, `head -n`, `tail -n`, and `sed -n` patterns when the target file looks like a real session transcript and the requested read is large enough to be token-expensive.
- Regression coverage now locks the new session-read rewrite behavior in both direct/unit tests and a compiled CLI rewrite integration test.

## 0.1.31

- `tsift diff-digest` now supports `--cached` for staged-index review and `--revision <rev>` for single-commit/history review, while keeping the existing working-tree mode as the default.
- `tsift rewrite` now auto-routes `git diff --cached`, `git show`, and simple patch-style `git log -p -1 ...` commands into the bounded diff-digest surface instead of letting raw non-working-tree hunks spill into agent context.
- Regression coverage now locks the new staged/revision digest behavior in both direct/unit tests and compiled CLI integration tests.

## 0.1.30

- Added `tsift session-digest`, a bounded transcript digest for markdown session docs and Claude JSONL. It extracts prompt targets, shell commands, touched files/symbols, failures, and closeout evidence such as verification/install/commit/push/version mentions.
- `session-digest` auto-detects markdown versus JSONL by default, supports explicit `--source markdown|jsonl`, and stays transcript-only instead of replaying tool calls or attempting full conversation reconstruction.
- Regression coverage now locks the direct helper logic, CLI parsing, and compiled CLI stdin path for the new command.

## 0.1.29

- Added `tsift metric-digest`, a generic metric-run digest for repeated benchmark/test/perf-style workflows. It reads JSON/NDJSON run history from stdin or `--input`, compares the latest run against a prior run or `--baseline`, and emits compact deltas plus markdown-ready history tables.
- `metric-digest` infers common metric directions (`mae`, `latency`, `cost`, `accuracy`, `score`, etc.), supports explicit `--metric`, `--lower-is-better`, and `--higher-is-better` overrides, and surfaces top improvements/regressions without hard-coding any session-share-specific schema.
- Regression coverage now locks the direct helper logic, CLI parsing, and compiled CLI stdin path for the new command.

## 0.1.28

- `tsift rewrite` now auto-routes plain `git diff` to `tsift diff-digest`, `cargo test` / `pytest` to a tsift-owned test-digest wrapper, and common verbose cargo build/check/clippy/install commands to a log-digest wrapper instead of leaving those high-token commands raw by default.
- The new hidden `tsift __digest-runner` helper executes the wrapped shell command, digests the captured stdout/stderr through `test-digest` or `log-digest`, and preserves the original exit code so failing tests/builds still fail closed.
- Regression coverage now locks the rewrite rules plus the digest-runner exit-code behavior in both unit tests and compiled CLI integration tests.

## 0.1.27

- Added `tsift log-digest`, a bounded verbose-log digest that reads captured stdout/stderr from stdin or `--input`, collapses repeated lines, groups warning/error signals, extracts file anchors and stack blocks, and emits JSON or compact human output.
- `log-digest` keeps summary enrichment read-only: when `.tsift/summaries.db` already has current rows for anchored files or extracted symbols, the digest includes up to two cached summary snippets; otherwise it degrades to `missing`, `stale`, or `unavailable` without mutating the cache.
- Regression coverage now locks this behavior in both the direct helper surface and the compiled CLI stdin path.

## 0.1.26

- Added `tsift test-digest`, a bounded test-output digest that reads captured runner output from stdin or `--input`, auto-detects `cargo`/`pytest` formats, groups duplicate failures, preserves file/line anchors, and emits JSON or compact human output.
- `test-digest` keeps summary enrichment read-only: when `.tsift/summaries.db` already has current rows for an anchored file, the digest includes up to two cached summary snippets; otherwise it degrades to `missing`, `stale`, or `unavailable` without mutating the cache.
- Regression coverage now locks this behavior in both the direct helper surface and the compiled CLI stdin path.

## 0.1.25

- Added `tsift diff-digest`, a bounded diff-adjacent report that compares `HEAD` to the working tree (plus untracked files) and emits changed files, touched symbols, current cached summary snippets when available, and added/removed call edges.
- `diff-digest` does not require a fresh `index.db`; it parses the changed file snapshots directly so unindexed working-tree edits still show up in the digest.
- Regression coverage now locks this behavior in both the direct helper surface and the compiled CLI command.

## 0.1.24

- Plain `tsift search <query>` now auto-promotes single-token identifier/path-like queries such as `claudescore-3`, `alpha_helper`, `src/main.rs`, and `crate::module` to the exact `rg -F` backend even when the caller does not spell `--exact`.
- That keeps the fast literal lookup path on by default for the query shapes that lexical BM25 tokenization handles worst, while still leaving plain word and multi-word prose searches on the lexical path.
- Native content/FTS indexing remains deferred for now because the main remaining lookup gap was backend selection, not missing indexed content storage.
- Regression coverage now locks this behavior in both the direct command path and the compiled CLI search surface.

## 0.1.23

- `tsift search --exact` now routes literal lookups through a first-class `rg -F` backend instead of sending every rg-style query through lexical BM25, so identifier-like searches such as `claudescore-3` return direct file hits without paying sift corpus/BM25 startup cost.
- Exact searches bypass the lexical stale-index precheck and the workspace shared-root-index requirement, so they still work when `.tsift/index.db` is stale/missing or when a workspace only has scoped `.tsift/indexes/<scope>/index.db` files.
- The `tsift rewrite` hook now rewrites `rg ...` and `grep -r ...` commands to `tsift search --exact ...`, preserving the fast literal-search path instead of silently translating those commands into lexical search.
- Regression coverage now locks this behavior in the direct exact-search helpers, the CLI parser, and the rewrite surface.

## 0.1.22

- `tsift search` now routes both in-process lexical searches and the timed `__search-worker` helper through a stable `.tsift/search-cache` directory rooted at the resolved project/workspace root, so repeated searches can reuse sift corpus/BM25 artifacts instead of rebuilding them from scratch every run.
- Scoped and federated searches share that same root-owned cache location rather than creating ad hoc caches under nested paths, so workspace searches keep their reusable search state under the canonical `.tsift/` tree.
- Regression coverage now locks this behavior in both the direct search helpers and the compiled CLI search surface.
- `tsift search` timeout diagnostics now re-check the same index targets after a worker timeout. Fresh indexes stop getting the misleading "index may be stale" hint, while indexes that became stale or disappeared mid-search now get a concrete reindex command in the timeout error itself.
- Regression coverage now locks this behavior in both the direct timeout helper and the compiled CLI search surface.
- `tsift status` now derives its `tsift summarize --extract ...` follow-up from the indexed layout instead of hardcoding `src/`, so root-level repos recommend `.` and workspace layouts only keep `src/` when that is the real shared scope prefix.
- Regression coverage now locks this behavior in the direct status helpers for single-root, `src/`-rooted, and mixed workspace layouts.
- `tsift status` now auto-builds missing workspace scoped indexes before it prints the final report, so a workspace with checked-out submodules but absent `.tsift/indexes/<scope>/index.db` files can recover to a completed status in one command instead of stopping at `index: missing` / `stale`.
- That auto-repair path only fills the missing scoped indexes; the low-level `status::check_status` helper remains read-only and stale-file reporting still stays visible after the rebuild pass.
- Regression coverage now locks this behavior in both the direct command path and the compiled CLI `status --json` surface.
- Read-only `index.db` and `summaries.db` recovery is now WAL-aware end to end: when a live SQLite lock blocks reads and `-wal` / `-shm` sidecars are present, tsift copies that live sidecar state into the snapshot fallback instead of copying only the main `.db` file or waiting for a rollback-journal marker that never appears in normal WAL mode.
- `tsift status` / `tsift locks` now report WAL sidecar presence explicitly and distinguish `snapshot_fallback_wal` recovery from the older rollback-journal snapshot path, so lock diagnostics describe the real live lock mode instead of implying every fallback came from `*.db-journal`.
- Regression coverage now locks this behavior in the shared read-only helpers, the direct status/summary readers, and compiled CLI `status` plus `summarize --stats` flows under a live WAL writer.

## 0.1.21

- Plain `tsift search` on a workspace root no longer auto-creates `.tsift/index.db` when the workspace only has scoped `.tsift/indexes/<scope>/index.db` files. It now fails closed and requires the caller to pick `--scope <scope>` or `--federated`.
- The new workspace-search error lists both the available scope ids and the currently indexed scope ids, so agents can choose the right search target without guessing or mutating the workspace layout by accident.
- Regression coverage now locks this behavior in both the direct command path and the compiled CLI search surface.
- Read-only summary queries (`tsift summarize --stats`, `tsift summarize <symbol>`, `tsift summarize --file <path>`) now retry against a snapshot copy when a rollback-journal lock wedges the live `summaries.db`, instead of surfacing a raw `database is locked` failure.
- `tsift status` summary coverage checks now use that same resilient summary read path and expose `recovery: snapshot_fallback` / `summaries recovery: ...` diagnostics when they had to degrade off the live cache.
- Regression coverage now locks this behavior in the low-level summary reader, the direct summarize/status command paths, and the compiled CLI summarize surface.

## 0.1.20

- `tsift status` now treats workspace scoped indexes as authoritative whenever `.gitmodules` defines scopes, even if a shared `.tsift/index.db` also exists, so missing scoped DBs can no longer masquerade as a fresh workspace.
- Mixed root-plus-scoped workspace status now keeps reporting `workspace_scopes` and `missing_scopes`, and the top-level recommendation continues to point at `tsift index --workspace .` instead of the shared-root path.
- Regression coverage now locks this behavior in both the direct status helpers and the compiled CLI status surface.

## 0.1.19

- `tsift status`, `tsift search`, and the read-only graph query commands now resolve nested input paths against the nearest ancestor project root that already owns `.tsift/`, instead of treating subdirectories as brand-new projects.
- Nested-path query calls therefore reuse the existing root or scoped indexes and stop auto-creating stray subdirectory `.tsift/index.db` state during search autoindex.
- Regression coverage now locks this behavior in the shared path-resolution helper, the direct command paths, and the compiled CLI query/status surface.

## 0.1.18

- `tsift summarize --extract <path> --diff` now includes untracked files under the requested extract scope, instead of only re-extracting tracked paths reported by `git diff --name-only HEAD`.
- Diff extraction now skips deleted paths before the summarize walk, so `--diff` only feeds readable source files into the extraction batch.
- Regression coverage now locks this behavior in the direct summarize diff path and the compiled CLI summarize surface.

## 0.1.17

- `tsift graph`, `tsift communities`, `tsift path`, and `tsift explain` now fail closed on workspace roots that only have scoped `.tsift/indexes/<scope>/index.db` state, instead of pointing at a missing `.tsift/index.db` and hiding the real fix.
- The new error explicitly requires `--scope <scope>` and lists both the available scope ids and the currently indexed scopes, so agents can pick a valid workspace query target without guessing.
- Regression coverage now locks this behavior in both the direct command path and the compiled CLI query surface.

## 0.1.16

- `tsift status` no longer reports a partially indexed workspace as `fresh`. If some configured scoped `index.db` files are missing, full-workspace misses remain `index: missing` while partial workspaces surface as `index: stale` with explicit `missing_scopes`.
- Workspace status output and `--json` now list the missing scope ids directly, so agents can distinguish "files changed" from "this configured submodule has never been indexed yet."
- Regression coverage now locks this behavior in both the direct status helpers and the compiled CLI status surface.

## 0.1.15

- `tsift index --submodule <name>` now uses the same strict workspace scope resolution as `--scope`, so unknown selectors fail closed instead of indexing `root/<name>` into an unreachable scoped database.
- Ambiguous duplicate leaf-name selectors now fail closed for submodule indexing too, requiring the concrete scope id when `.gitmodules` contains colliding leaf names.
- Regression coverage now locks this behavior in both the direct `cmd_index` path and the compiled CLI index surface.

## 0.1.14

- `tsift status` now detects workspace-only indexes under `.tsift/indexes/<scope>/index.db` instead of reporting `index: missing` whenever the root `.tsift/index.db` is absent.
- Workspace status output now reports the indexed scopes explicitly, aggregates their freshness into the top-level `index` state, and recommends `tsift index --workspace .` / `tsift init --workspace` for workspace roots.
- Regression coverage now locks this behavior in both the direct status helpers and the compiled CLI status surface.

## 0.1.13

- Workspace scope identifiers now stay unique even when `.gitmodules` contains duplicate trailing directory names. Unique leaves still use the short leaf name (for example `alpha`), but duplicate leaves promote to the full submodule path (for example `pkg/app/foo`, `vendor/foo`) so indexing and scoped search no longer collide onto the same `index.db`.
- Ambiguous legacy leaf selectors now fail closed and list the concrete scope ids to use, instead of silently resolving to whichever duplicate scope happened to win first.
- Regression coverage now locks this behavior in config parsing, in-process workspace search, workspace indexing, and the compiled CLI search surface.

## 0.1.12

- Workspace `tsift summarize --extract ...` now resolves symbol context per extracted file, so files under `.tsift/indexes/<scope>/index.db` use the matching scoped index instead of whichever workspace index appears first.
- Summarize symbol preload now uses exact normalized file matches instead of suffix matching, preventing same-path collisions across scoped indexes and locking the prompt context to the intended file.
- Regression coverage now locks this behavior in the direct summarize helpers, the workspace summarize command path, and the compiled CLI summarize surface.

## 0.1.10

- `tsift summarize --stats`, `tsift summarize <symbol>`, and `tsift summarize --file <path>` now fail closed when `.tsift/summaries.db` is absent and otherwise open the summary cache read-only, so lookup paths no longer create or contend on the cache DB.
- Regression coverage now locks this behavior in both the direct `cmd_summarize` path and the compiled CLI summarize surface.

## 0.1.11

- `tsift summarize --extract <relative>` now resolves the walked extraction path against `--path` / the canonical project root instead of the caller's current working directory, so batch extraction targets the intended repo even when the CLI runs from elsewhere.
- Regression coverage now locks this behavior in both the helper-level summarize path resolution and the compiled CLI summarize surface.

## 0.1.9

- `tsift lint --index .tsift/indexes` now treats the scoped-index directory itself as a valid discovery root, so explicit per-submodule linting no longer ignores every `index.db`.
- Regression coverage now locks this behavior in both the helper-level entity discovery path and the compiled CLI lint surface.

## 0.1.8

- `tsift lint` now opens discovered `index.db` files through the shared read-only path with rollback-journal snapshot fallback, so markdown linting stays available while a live writer holds the database lock.
- Regression coverage now locks this behavior in both the helper-level entity-loading path and the compiled CLI lint surface.

## 0.1.7

- `tsift lint` now auto-discovers live `index.db` files from the nearest ancestor `.tsift` root, including scoped `.tsift/indexes/*/index.db` layouts, instead of probing the retired `symbols.db` paths.
- Regression coverage now locks this behavior in both the helper-level discovery path and the compiled CLI lint surface.

## 0.1.6

- `tsift search --scope <name>` now fails closed when the named submodule does not exist, and reports the available workspace scopes instead of silently falling back to a full-workspace lexical search.
- Regression coverage now locks this behavior in both the direct `cmd_search` path and the compiled CLI integration test surface.

## 0.1.5

- `tsift communities` now opens `index.db` through the same read-only path as `graph`, `path`, and `explain`, so it no longer acquires the `index.lock` writer sidecar for a read-only graph query.
- Regression coverage now holds a live writer lock and asserts that both the in-process command path and the compiled CLI still succeed for `tsift communities`.

## 0.1.4

- `tsift index --prune` now falls back to the same full file-mtime scan as normal incremental indexing, so file edits inside unchanged directories are still detected correctly.
- The `--prune` flag remains in place as a compatibility surface and reports prune stats, but active subtree skipping is suspended until tsift has a sound invalidation model that cannot miss in-place file edits.

## 0.1.3

- `tsift index` now records non-fatal warnings when a changed file cannot be read or when symbol/call extraction fails, instead of silently swallowing those `.ok()` paths.
- Those warnings are emitted on stderr from shared index-update flows and also carried in the structured `IndexSummary`, so manual indexing and search autoindex no longer hide partial extraction failures.

## 0.1.2

- Writable `index.db` opens now set and verify `PRAGMA wal_autocheckpoint=256`, so routine tsift writes checkpoint the WAL on an explicit budget instead of relying on SQLite defaults.
- Regression coverage now asserts the busy timeout, WAL journal mode, and explicit auto-checkpoint setting together.

## 0.1.1

- `tsift search --timeout` now runs the bounded sift search in an internal helper process and kills that worker on timeout, so timed-out searches no longer keep burning CPU in detached threads.
- `--timeout 0` still keeps search in-process for long-running sessions that explicitly opt out of the timeout.

## 0.1.0

- Initial private versioned release surface for the tsift CLI.
- Commands available: `index`, `search`, `graph`, `communities`, `path`, `explain`, `edit`, `route`, `rewrite`, `sql`, `audit`, `summarize`, `lint`, `status`, `init`.
- Global output controls available: `--compact`, `--pretty`, `--terse`, `--schema`, `--absolute`, `--tabular`.
- Project setup includes Code Navigation instruction injection plus optional Codex auto-reindex hook install via `tsift init --codex`.
- `tsift search` now fast-fails on stale existing indexes and adds `--autoindex` for hook-like one-off recovery in unhooked sessions.
- Writable index updates now use a sibling `index.lock` sidecar so concurrent `tsift index` / `tsift search --autoindex` writers fail fast with a tsift-owned lock message instead of raw SQLite lock errors.
- Instruction version markers: `tsift init` now embeds `v=X.Y.Z` in the `<!-- tsift:code-navigation -->` opening marker. `tsift status` reports `instructions: current|stale|missing` and recommends `tsift init` when the installed version differs from the marker version. Pre-versioned markers (no `v=` attribute) are treated as stale.
