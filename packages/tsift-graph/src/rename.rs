//! Identifier occurrence collection for symbol renames.
//!
//! A rename used to be a substring scan with an identifier-boundary guard. That
//! shape cannot tell an identifier from the same characters inside a string
//! literal or a comment, so `rename_symbol` silently rewrote both — a string
//! literal is data, and rewriting it changes behaviour rather than names.
//!
//! Here the walk is restricted to the node kinds that *are* identifiers in each
//! grammar. Comments and string bodies are different node kinds, so they drop
//! out by construction; there is no comment or string special case below, and a
//! new quoting or comment form cannot reintroduce the bug.
//!
//! A kind filter alone is still coarser than the language: several grammars
//! spell two unrelated declarations with the same node kind — a Rust struct
//! field and a method call are both `field_identifier`, a GDScript `func` and a
//! local `var` are both `name`. Where the *position* in the tree separates
//! them, [`RenameTarget`] carries what the index resolved the symbol to be and
//! the walk drops occurrences that cannot be that thing. Where position does
//! not separate them, the occurrence is kept: under-renaming leaves a caller
//! pointing at a name that no longer exists, which is worse than the
//! over-renaming it would avoid.

use crate::lang::Lang;
use anyhow::Result;
use tree_sitter::{Node, Parser};

/// What the index resolved the rename target to be.
///
/// Grammars distinguish a declaration from a reference far more often than they
/// distinguish two same-named declarations, so this is the only input that lets
/// the walk tell `fn count()` from `struct S { count: usize }`. It comes from
/// the indexed symbol's kind, and [`RenameTarget::Unresolved`] — the default
/// when nothing resolved — accepts every identifier kind, which is exactly the
/// behaviour before this existed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum RenameTarget {
    /// A function or method.
    Callable,
    /// A GDScript `signal`. Declared and connected by name, but never a
    /// function, and the two have distinct declaration nodes.
    Signal,
    /// A type-level name: struct, enum, trait, class, interface, type alias.
    Type,
    /// A value binding: const, static, or variable.
    Value,
    /// Unresolved, or an indexed kind that maps to none of the above.
    #[default]
    Unresolved,
}

impl RenameTarget {
    /// Map an indexed symbol kind onto what the grammar can check.
    ///
    /// The input strings are the capture names in `Lang::symbol_query`, so an
    /// unrecognized one means a new capture was added without deciding what it
    /// is. That falls to `Unresolved`, which is the permissive answer — a new
    /// symbol kind must not silently start dropping occurrences.
    pub fn from_indexed_kind(kind: &str) -> Self {
        match kind {
            "function" | "method" => Self::Callable,
            "signal" => Self::Signal,
            "struct" | "enum" | "enum_class" | "trait" | "class" | "data_class"
            | "sealed_class" | "interface" | "type_alias" | "union" | "object"
            | "companion_object" | "impl" => Self::Type,
            "const" | "static" | "variable" => Self::Value,
            _ => Self::Unresolved,
        }
    }
}

/// The byte span of one identifier occurrence, as a half-open range.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IdentifierOccurrence {
    pub start_byte: usize,
    pub end_byte: usize,
    /// This occurrence is a JS-like object-literal shorthand (`{ beta }`),
    /// where the one token is both the property name and a read of the binding.
    /// Overwriting the span would silently rename the property too, so the
    /// splice writes `beta: newName` and both survive.
    pub expands_shorthand_key: bool,
}

/// Node kinds that carry a bare identifier in this language's grammar.
///
/// An empty slice means the language has no identifier concept a rename could
/// target (Markdown), which callers must treat as "not renamable" rather than
/// as "no occurrences found".
pub fn identifier_node_kinds(lang: Lang) -> &'static [&'static str] {
    match lang {
        // Rust identifiers inside macro arguments live under an opaque
        // `token_tree`, but they are still named `identifier` nodes, so a
        // `foo()` call inside `assert_eq!`/`format!` is reached by this walk.
        #[cfg(feature = "lang-rust")]
        Lang::Rust => &[
            "identifier",
            "type_identifier",
            "field_identifier",
            "shorthand_field_identifier",
        ],
        #[cfg(feature = "lang-python")]
        Lang::Python => &["identifier"],
        #[cfg(feature = "lang-typescript")]
        Lang::TypeScript | Lang::Tsx => &[
            "identifier",
            "type_identifier",
            "property_identifier",
            "shorthand_property_identifier",
            "shorthand_property_identifier_pattern",
        ],
        #[cfg(feature = "lang-javascript")]
        Lang::JavaScript | Lang::Jsx => &[
            "identifier",
            "property_identifier",
            "shorthand_property_identifier",
            "shorthand_property_identifier_pattern",
        ],
        #[cfg(feature = "lang-kotlin")]
        Lang::Kotlin => &["identifier"],
        #[cfg(feature = "lang-zig")]
        Lang::Zig => &["identifier"],
        // Bash has no separate identifier node: a command name and a function
        // name are both `word`, and an expansion is `variable_name`. `word` is
        // also every unquoted argument, so kind alone is not enough here —
        // `occurrence_is_renamable` narrows it to the name positions.
        #[cfg(feature = "lang-bash")]
        Lang::Bash => &["word", "variable_name"],
        // GDScript splits the two: `name` is the declared name of a statement
        // or block, `identifier` is every reference to one.
        #[cfg(feature = "lang-gdscript")]
        Lang::GdScript => &["identifier", "name"],
        // Markdown has headings, not identifiers; `rename_heading` is its kind.
        #[cfg(feature = "lang-markdown")]
        Lang::Markdown => &[],
    }
}

/// Whether an identifier-kind node sits in a *naming* position.
///
/// For most grammars the node kind settles it, and this is unconditionally
/// true. Bash is the exception that forces the check to exist: a bare `word`
/// is the function name in `deploy() { … }`, the command name in `deploy`,
/// **and** every unquoted argument, so `echo deploy` would otherwise have a
/// rename rewrite an argument that is data. Restricting `word` to the
/// declaration and command-name positions keeps arguments out, the same way
/// the kind filter keeps strings and comments out for every other language.
fn occurrence_is_renamable(lang: Lang, node: Node) -> bool {
    match lang {
        #[cfg(feature = "lang-bash")]
        Lang::Bash => {
            if node.kind() != "word" {
                // `variable_name` is only ever a variable, in an assignment or
                // an expansion.
                return true;
            }
            node.parent().is_some_and(|parent| {
                matches!(parent.kind(), "function_definition" | "command_name")
            })
        }
        _ => {
            let _ = node;
            true
        }
    }
}

/// Whether an identifier-kind node could be *this* symbol.
///
/// Only positions the grammar makes unambiguous are ruled out. Anything the
/// tree cannot attribute — a bare `count` reference in GDScript, a Rust
/// `x.count()` where `count` might be an inherent method or a trait method on
/// something else — is kept, because dropping it silently breaks a caller.
#[allow(unused_variables)]
fn occurrence_matches_target(
    lang: Lang,
    node: Node,
    source: &[u8],
    target: RenameTarget,
) -> bool {
    if target == RenameTarget::Unresolved {
        return true;
    }
    match lang {
        #[cfg(feature = "lang-rust")]
        Lang::Rust => rust_occurrence_matches_target(node, target),
        #[cfg(feature = "lang-python")]
        Lang::Python => python_occurrence_matches_target(node, source, target),
        #[cfg(feature = "lang-gdscript")]
        Lang::GdScript => gdscript_occurrence_matches_target(node, target),
        #[cfg(feature = "lang-typescript")]
        Lang::TypeScript | Lang::Tsx => js_like_occurrence_matches_target(node, target),
        #[cfg(feature = "lang-javascript")]
        Lang::JavaScript | Lang::Jsx => js_like_occurrence_matches_target(node, target),
        #[cfg(feature = "lang-kotlin")]
        Lang::Kotlin => kotlin_occurrence_matches_target(node, source, target),
        #[cfg(feature = "lang-zig")]
        Lang::Zig => zig_occurrence_matches_target(node, source, target),
        _ => {
            let _ = node;
            true
        }
    }
}

/// Python uses `identifier` for both a binding and the attribute in `obj.name`.
/// The attribute cannot be a module-level binding, except in two positions:
/// methods are indexed as callables, so `obj.name()` is a real rename target,
/// and `mod.name` is the module-level binding itself when `mod` is a module
/// this file imported. Dropping that second case is a silent under-rename —
/// `import mod` is half of how Python spells a cross-module reference, and the
/// rename runs across files.
#[cfg(feature = "lang-python")]
fn python_occurrence_matches_target(node: Node, source: &[u8], target: RenameTarget) -> bool {
    let Some(attribute) = node.parent().filter(|parent| parent.kind() == "attribute") else {
        return true;
    };
    if attribute
        .child_by_field_name("attribute")
        .is_none_or(|name| name.id() != node.id())
    {
        return true;
    }
    if python_receiver_is_imported_module(attribute, source) {
        return true;
    }

    target == RenameTarget::Callable
        && attribute.parent().is_some_and(|call| {
            call.kind() == "call"
                && call
                    .child_by_field_name("function")
                    .is_some_and(|function| function.id() == attribute.id())
        })
}

/// Whether the receiver of this `attribute` is a module bound by `import`.
///
/// Only `import mod` / `import pkg.mod as alias` bind a name that is reached
/// with a dot; `from mod import name` binds `name` directly and never produces
/// an attribute position. A chained receiver (`pkg.sub.name`) is resolved by
/// walking to the root of the chain, which is the imported name.
///
/// A local variable that shadows an imported module resolves to "module" here
/// and keeps the occurrence. That is the over-renaming direction, which this
/// module prefers: an extra rename is visible, a dropped one is not.
#[cfg(feature = "lang-python")]
fn python_receiver_is_imported_module(attribute: Node, source: &[u8]) -> bool {
    let Some(mut object) = attribute.child_by_field_name("object") else {
        return false;
    };
    while object.kind() == "attribute" {
        let Some(inner) = object.child_by_field_name("object") else {
            return false;
        };
        object = inner;
    }
    if object.kind() != "identifier" {
        return false;
    }
    let Ok(name) = object.utf8_text(source) else {
        return false;
    };
    python_file_imports_module(attribute, name, source)
}

/// Whether the file holding `node` binds `name` with an `import` statement.
#[cfg(feature = "lang-python")]
fn python_file_imports_module(node: Node, name: &str, source: &[u8]) -> bool {
    let mut root = node;
    while let Some(parent) = root.parent() {
        root = parent;
    }
    let mut cursor = root.walk();
    let mut descend = true;
    loop {
        if descend {
            let current = cursor.node();
            if current.kind() == "import_statement"
                && python_import_binds(current, name, source)
            {
                return true;
            }
            if cursor.goto_first_child() {
                continue;
            }
        }
        if cursor.goto_next_sibling() {
            descend = true;
            continue;
        }
        if !cursor.goto_parent() {
            return false;
        }
        descend = false;
    }
}

/// The name one `import_statement` clause binds: the alias when there is one,
/// otherwise the first segment of the dotted path — `import pkg.mod` binds
/// `pkg`, not `mod`.
#[cfg(feature = "lang-python")]
fn python_import_binds(import: Node, name: &str, source: &[u8]) -> bool {
    let mut cursor = import.walk();
    import.named_children(&mut cursor).any(|clause| {
        let bound = match clause.kind() {
            "aliased_import" => clause.child_by_field_name("alias"),
            "dotted_name" => clause.named_child(0),
            _ => None,
        };
        bound.is_some_and(|bound| bound.utf8_text(source).is_ok_and(|text| text == name))
    })
}

/// Kotlin's first `navigation_expression` identifier is the receiver binding;
/// every later identifier is a member. A member is a rename target in the callee
/// position, because Kotlin indexes methods as callables, and whenever the
/// receiver is a type declared in this file — `Panel.widgetCount` reaches a
/// companion member and `Registry.widgetCount` an `object` member, both of which
/// the index holds as declarations, so dropping them is an under-rename.
///
/// A receiver declared in another file is not resolvable here and falls to the
/// callee rule. Kotlin's ordinary cross-file reference is an `import` that binds
/// the name into scope as a bare identifier, which this walk never touches, so
/// that residue is the qualified-access case rather than the common one.
#[cfg(feature = "lang-kotlin")]
fn kotlin_occurrence_matches_target(node: Node, source: &[u8], target: RenameTarget) -> bool {
    let Some(navigation) = node
        .parent()
        .filter(|parent| parent.kind() == "navigation_expression")
    else {
        return true;
    };
    if node.prev_named_sibling().is_none() {
        return true;
    }
    if kotlin_receiver_is_declared_type(navigation, source) {
        return true;
    }

    target == RenameTarget::Callable
        && navigation.parent().is_some_and(|call| {
            call.kind() == "call_expression"
                && call
                    .named_child(0)
                    .is_some_and(|function| function.id() == navigation.id())
        })
}

/// Whether the receiver of this `navigation_expression` names a type declared in
/// this file, which makes the member a declaration rather than a value's field.
#[cfg(feature = "lang-kotlin")]
fn kotlin_receiver_is_declared_type(navigation: Node, source: &[u8]) -> bool {
    let mut receiver = navigation;
    while receiver.kind() == "navigation_expression" {
        let Some(inner) = receiver.named_child(0) else {
            return false;
        };
        receiver = inner;
    }
    if receiver.kind() != "identifier" {
        return false;
    }
    let Ok(name) = receiver.utf8_text(source) else {
        return false;
    };
    kotlin_file_declares_type(navigation, name, source)
}

/// Whether the file holding `node` declares a class, interface, or object named
/// `name`.
#[cfg(feature = "lang-kotlin")]
fn kotlin_file_declares_type(node: Node, name: &str, source: &[u8]) -> bool {
    let mut root = node;
    while let Some(parent) = root.parent() {
        root = parent;
    }
    let mut cursor = root.walk();
    let mut descend = true;
    loop {
        if descend {
            let current = cursor.node();
            if matches!(
                current.kind(),
                "class_declaration" | "object_declaration" | "interface_declaration"
            ) && current
                .child_by_field_name("name")
                .and_then(|declared| declared.utf8_text(source).ok())
                == Some(name)
            {
                return true;
            }
            if cursor.goto_first_child() {
                continue;
            }
        }
        if cursor.goto_next_sibling() {
            descend = true;
            continue;
        }
        if !cursor.goto_parent() {
            return false;
        }
        descend = false;
    }
}

/// Zig spells a struct field declaration and every member access with the same
/// flat `identifier` kind as a binding, so position is the only separator.
///
/// The member of `x.name` is *not* treated the way Python and Kotlin members
/// are, because Zig has no import-into-namespace form: `@import("m.zig").name`
/// and `Type.name` are the only ways to reach another declaration, and both are
/// `field_expression` members. Dropping them by position would leave every
/// cross-file reference of a renamed `const`, type, or non-called function
/// pointing at a name that no longer exists. So a member is kept whenever its
/// receiver chain roots in a *namespace* — an `@import` binding or a container
/// type — and dropped only when the receiver is an ordinary value, where the
/// member is a struct field. The callee exception applies there for the same
/// reason it does elsewhere: Zig indexes methods as `function_declaration`.
#[cfg(feature = "lang-zig")]
fn zig_occurrence_matches_target(node: Node, source: &[u8], target: RenameTarget) -> bool {
    let Some(parent) = node.parent() else {
        return true;
    };
    match parent.kind() {
        // `container_field` is a struct/enum/union field declaration. No
        // capture in `Lang::symbol_query` produces one, so it can never be the
        // symbol a resolved rename selected.
        "container_field" => parent
            .child_by_field_name("name")
            .is_none_or(|name| name.id() != node.id()),
        "field_expression" => {
            if parent
                .child_by_field_name("member")
                .is_none_or(|member| member.id() != node.id())
            {
                return true;
            }
            if zig_receiver_is_namespace(parent, source) {
                return true;
            }
            target == RenameTarget::Callable
                && parent.parent().is_some_and(|call| {
                    call.kind() == "call_expression"
                        && call
                            .child_by_field_name("function")
                            .is_some_and(|function| function.id() == parent.id())
                })
        }
        _ => true,
    }
}

/// Whether the receiver of `field_expression` is a namespace rather than a value.
///
/// `@import("m.zig").name` is a namespace outright. An identifier receiver is a
/// namespace when this file binds it to an `@import` or to a container type —
/// `const m = @import("m.zig")`, `const Panel = struct { ... }` — because a Zig
/// container type doubles as the namespace holding its declarations. A chained
/// receiver (`m.Sub.name`) is resolved by walking to the root of the chain.
///
/// Anything this cannot prove is *not* a namespace, which is the conservative
/// answer only because the caller's fallback for a value receiver still keeps
/// the callee position. A receiver whose binding lives in another file resolves
/// to `false` here; that case is the struct-field reading it is indistinguishable
/// from, and the call site is still renamed.
#[cfg(feature = "lang-zig")]
fn zig_receiver_is_namespace(field_expression: Node, source: &[u8]) -> bool {
    let Some(mut object) = field_expression.child_by_field_name("object") else {
        return false;
    };
    while object.kind() == "field_expression" {
        let Some(inner) = object.child_by_field_name("object") else {
            return false;
        };
        object = inner;
    }
    match object.kind() {
        "builtin_function" => zig_is_import_builtin(object, source),
        "identifier" => object
            .utf8_text(source)
            .is_ok_and(|name| zig_file_binds_namespace(field_expression, name, source)),
        _ => false,
    }
}

/// Whether this `builtin_function` node is an `@import(...)` call.
#[cfg(feature = "lang-zig")]
fn zig_is_import_builtin(builtin: Node, source: &[u8]) -> bool {
    let mut cursor = builtin.walk();
    builtin.named_children(&mut cursor).any(|child| {
        child.kind() == "builtin_identifier"
            && child.utf8_text(source).is_ok_and(|text| text == "@import")
    })
}

/// Whether the file holding `node` binds `name` to an `@import` or a container
/// type declaration.
///
/// Only whole-file scanning can answer this, and it runs once per *matching*
/// occurrence — the walk has already filtered to identifiers whose text is the
/// symbol being renamed — so it is bounded by the number of member positions
/// that spell the renamed name, not by the file's identifier count.
#[cfg(feature = "lang-zig")]
fn zig_file_binds_namespace(node: Node, name: &str, source: &[u8]) -> bool {
    let mut root = node;
    while let Some(parent) = root.parent() {
        root = parent;
    }
    let mut cursor = root.walk();
    let mut descend = true;
    loop {
        if descend {
            let current = cursor.node();
            if current.kind() == "variable_declaration"
                && zig_declaration_binds_namespace(current, name, source)
            {
                return true;
            }
            if cursor.goto_first_child() {
                continue;
            }
        }
        if cursor.goto_next_sibling() {
            descend = true;
            continue;
        }
        if !cursor.goto_parent() {
            return false;
        }
        descend = false;
    }
}

/// Whether one `variable_declaration` binds `name` to a namespace value.
#[cfg(feature = "lang-zig")]
fn zig_declaration_binds_namespace(declaration: Node, name: &str, source: &[u8]) -> bool {
    let mut cursor = declaration.walk();
    let children: Vec<Node> = declaration.named_children(&mut cursor).collect();
    let binds_name = children.iter().any(|child| {
        child.kind() == "identifier" && child.utf8_text(source).is_ok_and(|text| text == name)
    });
    if !binds_name {
        return false;
    }
    children.iter().any(|child| match child.kind() {
        "builtin_function" => zig_is_import_builtin(*child, source),
        // A Zig container type is also the namespace holding its declarations,
        // so `Panel.method` reaches a `function_declaration` the index has.
        "struct_declaration" | "enum_declaration" | "union_declaration"
        | "opaque_declaration" => true,
        _ => false,
    })
}

/// The JS-like grammars spell every property `property_identifier`, whether it
/// is an object-literal key, a class method, or a member access. None of those
/// is the module-level binding a rename resolves to — `Lang::symbol_query`
/// indexes `function_declaration`, `class_declaration`, and arrow-valued
/// `variable_declarator`, and nothing else — so a resolved rename must leave
/// them alone.
#[cfg(any(feature = "lang-typescript", feature = "lang-javascript"))]
fn js_like_occurrence_matches_target(node: Node, target: RenameTarget) -> bool {
    match node.kind() {
        "property_identifier" => false,
        "type_identifier" => target == RenameTarget::Type,
        _ => true,
    }
}

/// Whether this occurrence must be written as `key: replacement`.
///
/// `{ beta }` is one token doing two jobs: it names the property *and* reads
/// the binding. Overwriting the span renames the property as a side effect;
/// skipping it leaves a read of a name that no longer exists. Expanding to
/// `beta: gamma` is the only spelling where both stay correct, and it is
/// exactly what the shorthand desugars to.
#[allow(unused_variables)]
fn occurrence_expands_shorthand_key(lang: Lang, node: Node, target: RenameTarget) -> bool {
    if target == RenameTarget::Unresolved {
        return false;
    }
    match lang {
        #[cfg(feature = "lang-typescript")]
        Lang::TypeScript | Lang::Tsx => js_like_shorthand_key(node),
        #[cfg(feature = "lang-javascript")]
        Lang::JavaScript | Lang::Jsx => js_like_shorthand_key(node),
        _ => false,
    }
}

/// An object-literal shorthand, and deliberately *not* a destructuring pattern.
///
/// `const { beta } = mod` is `shorthand_property_identifier_pattern`: there the
/// token reads a property off `mod` and declares a local of the same name, so
/// the correct rewrite depends on whether `mod` is the module whose export was
/// renamed — which is the common case, and which plain span renaming already
/// gets right. Expanding it would be wrong for that case, so it is left alone.
#[cfg(any(feature = "lang-typescript", feature = "lang-javascript"))]
fn js_like_shorthand_key(node: Node) -> bool {
    node.kind() == "shorthand_property_identifier"
        && node.parent().is_some_and(|parent| parent.kind() == "object")
}

/// Rust spells three unrelated things `field_identifier`: a struct field
/// declaration, a field read, and the method in `x.method()`. The first two
/// cannot be a function, and the third must stay, or renaming a method would
/// leave every call site broken.
#[cfg(feature = "lang-rust")]
fn rust_occurrence_matches_target(node: Node, target: RenameTarget) -> bool {
    let parent_kind = node.parent().map(|parent| parent.kind()).unwrap_or("");
    match node.kind() {
        "field_identifier" => {
            // `x.count()` parses as a `call_expression` whose `function` is the
            // `field_expression` holding this node. Every other position — a
            // `field_declaration`, a `field_initializer`, a bare `x.count` read
            // — is a field, which a function/type/value rename must not touch.
            target == RenameTarget::Callable && parent_kind == "field_expression" && {
                node.parent()
                    .and_then(|field_expression| {
                        let call = field_expression.parent()?;
                        (call.kind() == "call_expression"
                            && call.child_by_field_name("function")?.id() == field_expression.id())
                        .then_some(())
                    })
                    .is_some()
            }
        }
        "shorthand_field_identifier" => target == RenameTarget::Value,
        // `S { count }` desugars to `count: count`, so the identifier names a
        // *field* as well as reading a binding. Renaming only the read would
        // change the field too, so a function or type rename skips it; a value
        // rename keeps the pre-existing behaviour.
        "identifier" if parent_kind == "shorthand_field_initializer" => {
            matches!(target, RenameTarget::Value)
        }
        "type_identifier" => target == RenameTarget::Type,
        _ => true,
    }
}

/// GDScript spells every declared name `name`, from `func` to a local `var`,
/// and every reference `identifier`. The declaration node therefore says which
/// kind of thing is being declared, and a rename of one kind must not rewrite
/// another's declaration.
#[cfg(feature = "lang-gdscript")]
fn gdscript_occurrence_matches_target(node: Node, target: RenameTarget) -> bool {
    let parent_kind = node.parent().map(|parent| parent.kind()).unwrap_or("");
    match node.kind() {
        "name" => {
            let declares: &[&str] = match target {
                RenameTarget::Callable => &["function_definition"],
                RenameTarget::Signal => &["signal_statement"],
                RenameTarget::Type => &["class_definition", "class_name_statement", "enum_definition"],
                RenameTarget::Value => &[
                    "variable_statement",
                    "const_statement",
                    "export_variable_statement",
                    "onready_variable_statement",
                ],
                RenameTarget::Unresolved => return true,
            };
            declares.contains(&parent_kind)
        }
        // A parameter is a fresh binding that shadows, never a reference to the
        // module-level symbol being renamed.
        "identifier" if parent_kind == "parameters" => false,
        _ => true,
    }
}

/// Every occurrence of `name` that is a real identifier node, in source order.
///
/// Returns an empty vector when the name never appears as an identifier, which
/// is distinct from it appearing only inside strings or comments — both look
/// the same to the caller, and both mean "there is nothing here to rename".
pub fn identifier_occurrences(
    lang: Lang,
    source: &[u8],
    name: &str,
) -> Result<Vec<IdentifierOccurrence>> {
    identifier_occurrences_for(lang, source, name, RenameTarget::Unresolved)
}

/// The same walk, narrowed to occurrences that could be `target`.
pub fn identifier_occurrences_for(
    lang: Lang,
    source: &[u8],
    name: &str,
    target: RenameTarget,
) -> Result<Vec<IdentifierOccurrence>> {
    let kinds = identifier_node_kinds(lang);
    if kinds.is_empty() || name.is_empty() {
        return Ok(Vec::new());
    }

    let ts_lang = lang.tree_sitter_language();
    let mut parser = Parser::new();
    parser.set_language(&ts_lang)?;
    let tree = parser
        .parse(source, None)
        .ok_or_else(|| anyhow::anyhow!("parse failed"))?;

    let mut occurrences = Vec::new();
    // A declaration of the same name that the target narrowing rejected, and
    // that *shadows* the target rather than merely coexisting with it.
    let mut shadowing_declaration_line: Option<usize> = None;
    // A reference the grammar cannot attribute to either one.
    let mut saw_ambiguous_reference = false;
    let mut cursor = tree.walk();
    let mut descend = true;
    loop {
        if descend {
            let node = cursor.node();
            if kinds.contains(&node.kind())
                && node.utf8_text(source).is_ok_and(|it| it == name)
                && occurrence_is_renamable(lang, node)
            {
                if occurrence_matches_target(lang, node, source, target) {
                    occurrences.push(IdentifierOccurrence {
                        start_byte: node.start_byte(),
                        end_byte: node.end_byte(),
                        expands_shorthand_key: occurrence_expands_shorthand_key(
                            lang, node, target,
                        ),
                    });
                    saw_ambiguous_reference |= occurrence_is_ambiguous_reference(lang, node, target);
                } else if shadowing_declaration_line.is_none()
                    && occurrence_shadows_target(lang, node, target)
                {
                    shadowing_declaration_line = Some(node.start_position().row + 1);
                }
            }
            if cursor.goto_first_child() {
                continue;
            }
        }
        if cursor.goto_next_sibling() {
            descend = true;
            continue;
        }
        if !cursor.goto_parent() {
            break;
        }
        descend = false;
    }

    // A pre-order walk already yields these in source order, but nested
    // grammars can nest an identifier inside another identifier-kind node, and
    // every caller splices spans left to right.
    occurrences.sort_by_key(|occurrence| (occurrence.start_byte, occurrence.end_byte));
    occurrences.dedup();

    // Narrowing a declaration out while still rewriting references to it would
    // produce a file where the declaration keeps the old name and a read of it
    // carries the new one — internally inconsistent, and worse than either
    // renaming both or renaming neither. Where the grammar cannot separate the
    // two, refuse and name the shadow, the same way an unattributable
    // cross-file reference refuses instead of guessing.
    if let Some(line) = shadowing_declaration_line
        && saw_ambiguous_reference
    {
        anyhow::bail!(
            "rename_symbol refuses {name:?}: a same-named declaration on line {line} shadows it, and a bare reference cannot say which one it belongs to"
        );
    }
    Ok(occurrences)
}

/// A rejected declaration that *shadows* the rename target inside this file.
///
/// Two Rust `field_identifier` positions are not shadows: a field and a
/// function are reached through different syntax, so no reference is ambiguous.
/// A GDScript local `var` is a shadow: within its scope a bare `count` is the
/// variable, not the function, and the grammar spells both the same.
fn occurrence_shadows_target(lang: Lang, node: Node, target: RenameTarget) -> bool {
    match lang {
        #[cfg(feature = "lang-gdscript")]
        Lang::GdScript => {
            if target != RenameTarget::Callable {
                return false;
            }
            let parent_kind = node.parent().map(|parent| parent.kind()).unwrap_or("");
            match node.kind() {
                "name" => matches!(
                    parent_kind,
                    "variable_statement"
                        | "const_statement"
                        | "export_variable_statement"
                        | "onready_variable_statement"
                ),
                "identifier" => parent_kind == "parameters",
                _ => false,
            }
        }
        _ => {
            let _ = (node, target);
            false
        }
    }
}

/// A kept occurrence that a shadowing declaration would make ambiguous.
///
/// A callee is never ambiguous — `count()` is the function whatever else is in
/// scope. A bare read is, because it could be either.
fn occurrence_is_ambiguous_reference(lang: Lang, node: Node, target: RenameTarget) -> bool {
    match lang {
        #[cfg(feature = "lang-gdscript")]
        Lang::GdScript => {
            if target != RenameTarget::Callable || node.kind() != "identifier" {
                return false;
            }
            let parent_kind = node.parent().map(|parent| parent.kind()).unwrap_or("");
            !matches!(parent_kind, "call" | "attribute_call" | "base_call")
        }
        _ => {
            let _ = (node, target);
            false
        }
    }
}

/// Splice `replacement` over every occurrence span, returning the new source
/// and the number of substitutions.
pub fn replace_occurrences(
    source: &str,
    occurrences: &[IdentifierOccurrence],
    replacement: &str,
) -> (String, usize) {
    let mut out = String::with_capacity(source.len());
    let mut last = 0usize;
    let mut replaced = 0usize;
    for occurrence in occurrences {
        if occurrence.start_byte < last {
            // Overlapping spans would corrupt the splice; the first one wins.
            continue;
        }
        out.push_str(&source[last..occurrence.start_byte]);
        if occurrence.expands_shorthand_key {
            // `{ beta }` becomes `{ beta: gamma }`: the property keeps its name,
            // the value follows the rename.
            out.push_str(&source[occurrence.start_byte..occurrence.end_byte]);
            out.push_str(": ");
        }
        out.push_str(replacement);
        last = occurrence.end_byte;
        replaced += 1;
    }
    out.push_str(&source[last..]);
    (out, replaced)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(feature = "lang-rust")]
    const RUST_SOURCE: &str = r#"/// doc widget_count
fn widget_count() -> usize { 3 }

fn describe() -> String {
    // widget_count comment
    let label = "widget_count";
    format!("{label}: {}", widget_count())
}
"#;

    #[cfg(feature = "lang-rust")]
    #[test]
    fn rust_skips_strings_and_comments_but_reaches_macro_arguments() {
        let found =
            identifier_occurrences(Lang::Rust, RUST_SOURCE.as_bytes(), "widget_count").unwrap();
        // The definition and the call inside `format!` — not the doc comment,
        // the line comment, or the string literal.
        assert_eq!(
            found.len(),
            2,
            "expected the definition and the macro-argument call, got {found:?}"
        );
        for occurrence in &found {
            let before = &RUST_SOURCE[..occurrence.start_byte];
            assert!(
                !before.ends_with("/// doc ") && !before.ends_with("// "),
                "occurrence at {} is inside a comment",
                occurrence.start_byte
            );
            assert!(
                !before.ends_with('"'),
                "occurrence at {} is inside a string literal",
                occurrence.start_byte
            );
        }
    }

    #[cfg(feature = "lang-rust")]
    #[test]
    fn replacing_rust_occurrences_leaves_prose_and_data_alone() {
        let found =
            identifier_occurrences(Lang::Rust, RUST_SOURCE.as_bytes(), "widget_count").unwrap();
        let (out, replaced) = replace_occurrences(RUST_SOURCE, &found, "gadget_count");
        assert_eq!(replaced, 2);
        assert!(out.contains("fn gadget_count()"), "definition not renamed");
        assert!(
            out.contains("gadget_count())"),
            "macro-argument call not renamed"
        );
        assert!(
            out.contains("/// doc widget_count"),
            "doc comment was renamed"
        );
        assert!(
            out.contains("// widget_count comment"),
            "line comment was renamed"
        );
        assert!(
            out.contains("\"widget_count\""),
            "string literal was renamed"
        );
    }

    #[cfg(feature = "lang-python")]
    #[test]
    fn python_skips_strings_and_comments() {
        let source = "def widget_count():\n    # widget_count comment\n    return \"widget_count\"\n\nwidget_count()\n";
        let found = identifier_occurrences(Lang::Python, source.as_bytes(), "widget_count").unwrap();
        assert_eq!(found.len(), 2, "got {found:?}");
        let (out, replaced) = replace_occurrences(source, &found, "gadget_count");
        assert_eq!(replaced, 2);
        assert!(out.contains("def gadget_count()"));
        assert!(out.contains("gadget_count()\n"));
        assert!(out.contains("# widget_count comment"));
        assert!(out.contains("\"widget_count\""));
    }

    #[cfg(feature = "lang-python")]
    #[test]
    fn python_callable_narrowing_keeps_method_calls_but_skips_attribute_reads() {
        let source = "def widget_count():\n    return 1\n\nclass Panel:\n    def widget_count(self):\n        return 2\n\nread = panel.widget_count\ncalled = panel.widget_count()\ndirect = widget_count()\n";
        let found = identifier_occurrences_for(
            Lang::Python,
            source.as_bytes(),
            "widget_count",
            RenameTarget::Callable,
        )
        .unwrap();
        let (out, replaced) = replace_occurrences(source, &found, "gadget_count");

        assert_eq!(replaced, 4, "got {found:?}\n{out}");
        assert!(out.contains("def gadget_count():"));
        assert!(out.contains("def gadget_count(self):"));
        assert!(out.contains("called = panel.gadget_count()"));
        assert!(out.contains("direct = gadget_count()"));
        assert!(out.contains("read = panel.widget_count\n"));
    }

    /// The attribute rule must not swallow `mod.name`. `import mod` is half of
    /// how Python spells a cross-module reference, and the rename is cross-file,
    /// so dropping it renames the definition and leaves every reader broken.
    #[cfg(feature = "lang-python")]
    #[test]
    fn python_narrowing_keeps_imported_module_attributes_including_bare_reads() {
        let source = "import mod\nimport pkg.deep as aliased\n\ndef widget_count():\n    return 1\n\nread = panel.widget_count\nmodule_read = mod.widget_count\nmodule_call = mod.widget_count()\naliased_read = aliased.widget_count\n";
        let found = identifier_occurrences_for(
            Lang::Python,
            source.as_bytes(),
            "widget_count",
            RenameTarget::Callable,
        )
        .unwrap();
        let (out, replaced) = replace_occurrences(source, &found, "gadget_count");

        assert_eq!(replaced, 4, "got {found:?}\n{out}");
        assert!(out.contains("def gadget_count():"), "{out}");
        assert!(
            out.contains("module_read = mod.gadget_count\n"),
            "an imported-module read was dropped:\n{out}"
        );
        assert!(out.contains("module_call = mod.gadget_count()"), "{out}");
        assert!(
            out.contains("aliased_read = aliased.gadget_count"),
            "an aliased-import read was dropped:\n{out}"
        );
        assert!(
            out.contains("read = panel.widget_count\n"),
            "an instance attribute read was renamed:\n{out}"
        );
    }

    #[cfg(feature = "lang-kotlin")]
    #[test]
    fn kotlin_callable_narrowing_keeps_method_calls_but_skips_navigation_reads() {
        let source = "fun widgetCount(): Int = 1\n\nclass Panel {\n    fun widgetCount(): Int = 2\n}\n\nval read = panel.widgetCount\nval called = panel.widgetCount()\nval direct = widgetCount()\n";
        let found = identifier_occurrences_for(
            Lang::Kotlin,
            source.as_bytes(),
            "widgetCount",
            RenameTarget::Callable,
        )
        .unwrap();
        let (out, replaced) = replace_occurrences(source, &found, "gadgetCount");

        assert_eq!(replaced, 4, "got {found:?}\n{out}");
        assert!(out.contains("fun gadgetCount(): Int = 1"));
        assert!(out.contains("fun gadgetCount(): Int = 2"));
        assert!(out.contains("val called = panel.gadgetCount()"));
        assert!(out.contains("val direct = gadgetCount()"));
        assert!(out.contains("val read = panel.widgetCount\n"));
    }

    /// A receiver that names a declared type is a namespace, not a value, so its
    /// member is a declaration the index holds. Dropping it renames the
    /// companion/object declaration and leaves the qualified access behind.
    #[cfg(feature = "lang-kotlin")]
    #[test]
    fn kotlin_narrowing_keeps_members_of_types_declared_in_the_file() {
        let source = "class Panel {\n    companion object {\n        fun widgetCount(): Int = 2\n    }\n}\n\nobject Registry {\n    fun widgetCount(): Int = 3\n}\n\nval fromClass = Panel.widgetCount\nval fromObject = Registry.widgetCount()\nval fromValue = panel.widgetCount\n";
        let found = identifier_occurrences_for(
            Lang::Kotlin,
            source.as_bytes(),
            "widgetCount",
            RenameTarget::Callable,
        )
        .unwrap();
        let (out, replaced) = replace_occurrences(source, &found, "gadgetCount");

        assert_eq!(replaced, 4, "got {found:?}\n{out}");
        assert!(out.contains("fun gadgetCount(): Int = 2"), "{out}");
        assert!(out.contains("fun gadgetCount(): Int = 3"), "{out}");
        assert!(
            out.contains("val fromClass = Panel.gadgetCount\n"),
            "a companion member read was dropped:\n{out}"
        );
        assert!(
            out.contains("val fromObject = Registry.gadgetCount()"),
            "an object member call was dropped:\n{out}"
        );
        assert!(
            out.contains("val fromValue = panel.widgetCount\n"),
            "a value's member read was renamed:\n{out}"
        );
    }

    #[cfg(feature = "lang-typescript")]
    #[test]
    fn typescript_skips_strings_and_comments() {
        let source = "// widgetCount comment\nfunction widgetCount(): number { return 1; }\nconst label = \"widgetCount\";\nwidgetCount();\n";
        let found =
            identifier_occurrences(Lang::TypeScript, source.as_bytes(), "widgetCount").unwrap();
        assert_eq!(found.len(), 2, "got {found:?}");
        let (out, _) = replace_occurrences(source, &found, "gadgetCount");
        assert!(out.contains("function gadgetCount()"));
        assert!(out.contains("// widgetCount comment"));
        assert!(out.contains("\"widgetCount\""));
    }

    #[cfg(feature = "lang-bash")]
    const BASH_SOURCE: &str = r#"widget_count() {
  echo widget_count
  local label="widget_count"
  # widget_count comment
  echo "$widget_count"
}
widget_count
"#;

    #[cfg(feature = "lang-bash")]
    #[test]
    fn bash_renames_names_but_not_arguments_prose_or_data() {
        let found =
            identifier_occurrences(Lang::Bash, BASH_SOURCE.as_bytes(), "widget_count").unwrap();
        // The definition, the `$widget_count` expansion, and the bare call —
        // not the `echo widget_count` argument, the string, or the comment.
        assert_eq!(found.len(), 3, "got {found:?}");
        let (out, replaced) = replace_occurrences(BASH_SOURCE, &found, "gadget_count");
        assert_eq!(replaced, 3);
        assert!(out.contains("gadget_count() {"), "definition not renamed");
        assert!(
            out.contains("echo \"$gadget_count\""),
            "expansion not renamed"
        );
        assert!(
            out.contains("}\ngadget_count\n"),
            "bare call not renamed:\n{out}"
        );
        assert!(
            out.contains("echo widget_count\n"),
            "an unquoted argument was renamed, which rewrites data:\n{out}"
        );
        assert!(out.contains("label=\"widget_count\""), "string was renamed");
        assert!(
            out.contains("# widget_count comment"),
            "comment was renamed"
        );
    }

    #[cfg(feature = "lang-zig")]
    const ZIG_MEMBER_SOURCE: &str = "const m = @import(\"m.zig\");\n\npub fn widget_count() u32 { return 3; }\n\nconst Panel = struct {\n    widget_count: u32 = 0,\n\n    pub fn describe(self: Panel) u32 { return self.widget_count; }\n};\n\npub fn caller(p: Panel) u32 {\n    return widget_count() + p.widget_count + m.widget_count() + m.widget_count + Panel.widget_count;\n}\n";

    /// The member positions Zig cannot narrow by the callee rule alone. A field
    /// read off a value is dropped; a namespace member is kept whether or not
    /// it is called, because `@import(...)` and a container type are the only
    /// ways Zig reaches another declaration.
    #[cfg(feature = "lang-zig")]
    #[test]
    fn zig_callable_narrowing_keeps_namespace_members_but_skips_field_reads() {
        let found = identifier_occurrences_for(
            Lang::Zig,
            ZIG_MEMBER_SOURCE.as_bytes(),
            "widget_count",
            RenameTarget::Callable,
        )
        .unwrap();
        let (out, replaced) = replace_occurrences(ZIG_MEMBER_SOURCE, &found, "gadget_count");

        assert_eq!(replaced, 5, "got {found:?}\n{out}");
        assert!(out.contains("pub fn gadget_count() u32"), "{out}");
        assert!(out.contains("return gadget_count() +"), "{out}");
        assert!(out.contains("m.gadget_count()"), "import call dropped:\n{out}");
        assert!(
            out.contains("m.gadget_count +"),
            "import read dropped, which breaks every cross-file reference:\n{out}"
        );
        assert!(
            out.contains("Panel.gadget_count;"),
            "container-type member dropped:\n{out}"
        );
        assert!(
            out.contains("    widget_count: u32 = 0,"),
            "a struct field declaration was renamed:\n{out}"
        );
        assert!(
            out.contains("p.widget_count +"),
            "a field read off a value was renamed:\n{out}"
        );
        assert!(
            out.contains("return self.widget_count;"),
            "a field read off self was renamed:\n{out}"
        );
    }

    /// A `const` rename keeps the namespace members — those name a module-level
    /// declaration in another file — and drops the struct field: no capture in
    /// `Lang::symbol_query` produces a `container_field`, so a field is never the
    /// symbol a resolved rename selected, and a field read off a value receiver
    /// is the one member position the grammar does attribute.
    #[cfg(feature = "lang-zig")]
    #[test]
    fn zig_value_narrowing_keeps_namespace_members_and_drops_struct_fields() {
        let found = identifier_occurrences_for(
            Lang::Zig,
            ZIG_MEMBER_SOURCE.as_bytes(),
            "widget_count",
            RenameTarget::Value,
        )
        .unwrap();
        let (out, _) = replace_occurrences(ZIG_MEMBER_SOURCE, &found, "gadget_count");

        assert!(
            out.contains("m.gadget_count +"),
            "an import-qualified const read was dropped:\n{out}"
        );
        assert!(
            out.contains("Panel.gadget_count;"),
            "a container-type const read was dropped:\n{out}"
        );
        assert!(
            out.contains("p.widget_count +"),
            "a struct field read was renamed by a const rename:\n{out}"
        );
        assert!(
            out.contains("    widget_count: u32 = 0,"),
            "the field declaration is not an indexed symbol and must not move:\n{out}"
        );
    }

    #[cfg(feature = "lang-zig")]
    #[test]
    fn zig_skips_strings_and_comments() {
        let source = "// widget_count comment\npub fn widget_count() u32 {\n    const label = \"widget_count\";\n    _ = label;\n    return 3;\n}\npub fn caller() u32 { return widget_count(); }\n";
        let found = identifier_occurrences(Lang::Zig, source.as_bytes(), "widget_count").unwrap();
        assert_eq!(found.len(), 2, "got {found:?}");
        let (out, replaced) = replace_occurrences(source, &found, "gadget_count");
        assert_eq!(replaced, 2);
        assert!(out.contains("pub fn gadget_count()"), "definition not renamed");
        assert!(out.contains("return gadget_count();"), "call not renamed");
        assert!(
            out.contains("// widget_count comment"),
            "comment was renamed"
        );
        assert!(out.contains("\"widget_count\""), "string was renamed");
    }

    #[cfg(feature = "lang-gdscript")]
    #[test]
    fn gdscript_renames_declaration_and_reference_but_not_prose() {
        let source = "# widget_count comment\nfunc widget_count():\n\tvar label = \"widget_count\"\n\treturn label\n\nfunc caller():\n\treturn widget_count()\n";
        let found =
            identifier_occurrences(Lang::GdScript, source.as_bytes(), "widget_count").unwrap();
        // GDScript names a declaration with `name` and every reference with
        // `identifier`; the rename has to reach both kinds.
        assert_eq!(found.len(), 2, "got {found:?}");
        let (out, replaced) = replace_occurrences(source, &found, "gadget_count");
        assert_eq!(replaced, 2);
        assert!(out.contains("func gadget_count():"), "definition not renamed");
        assert!(out.contains("return gadget_count()"), "call not renamed");
        assert!(
            out.contains("# widget_count comment"),
            "comment was renamed"
        );
        assert!(out.contains("\"widget_count\""), "string was renamed");
    }

    #[cfg(feature = "lang-rust")]
    const RUST_FIELD_SOURCE: &str = r#"struct Meter { count: usize }
fn count() -> usize { 3 }
impl Meter {
    fn read(&self) -> usize { self.count }
    fn count(&self) -> usize { self.count }
}
fn use_it(m: &Meter) -> usize { m.count() + m.count + count() }
fn build() -> Meter { Meter { count: 1 } }
"#;

    #[cfg(feature = "lang-rust")]
    #[test]
    fn renaming_a_rust_function_leaves_an_identically_named_field_alone() {
        let found =
            identifier_occurrences_for(Lang::Rust, RUST_FIELD_SOURCE.as_bytes(), "count", RenameTarget::Callable)
                .unwrap();
        let (out, _) = replace_occurrences(RUST_FIELD_SOURCE, &found, "tally");
        // Renamed: the free fn, the inherent method, the method call, the call.
        assert!(out.contains("fn tally() -> usize"), "free fn:\n{out}");
        assert!(out.contains("fn tally(&self)"), "inherent method:\n{out}");
        assert!(out.contains("m.tally()"), "method call:\n{out}");
        assert!(out.contains("+ tally()"), "free call:\n{out}");
        // Untouched: every position that is a field, not a function.
        assert!(
            out.contains("struct Meter { count: usize }"),
            "field declaration was renamed:\n{out}"
        );
        assert!(
            out.contains("{ self.count }"),
            "field read was renamed:\n{out}"
        );
        assert!(
            out.contains("m.count +"),
            "field read was renamed:\n{out}"
        );
        assert!(
            out.contains("Meter { count: 1 }"),
            "struct literal field was renamed:\n{out}"
        );
    }

    #[cfg(feature = "lang-rust")]
    #[test]
    fn an_unresolved_rust_target_keeps_the_pre_narrowing_behaviour() {
        // With no resolved symbol there is nothing to narrow by, and dropping
        // occurrences on a guess would silently under-rename.
        let narrowed = identifier_occurrences_for(
            Lang::Rust,
            RUST_FIELD_SOURCE.as_bytes(),
            "count",
            RenameTarget::Callable,
        )
        .unwrap();
        let wide = identifier_occurrences(Lang::Rust, RUST_FIELD_SOURCE.as_bytes(), "count").unwrap();
        assert!(
            wide.len() > narrowed.len(),
            "narrowing dropped nothing: {} vs {}",
            wide.len(),
            narrowed.len()
        );
    }

    #[cfg(feature = "lang-rust")]
    #[test]
    fn a_field_access_inside_a_macro_is_still_renamed() {
        // Known limitation, pinned rather than left as folklore. tree-sitter
        // parses macro arguments as an opaque `token_tree`, so `m.count`
        // inside `format!` is a bare `identifier` with no `field_expression`
        // around it — the position rule has nothing to read. Over-renaming is
        // the deliberate side to err on: the alternative is dropping the real
        // call sites inside macros that the walk exists to reach.
        let source = "struct Meter { count: usize }\nfn count() -> usize { 3 }\nfn f(m: &Meter) -> String { format!(\"{}\", m.count) }\n";
        let found =
            identifier_occurrences_for(Lang::Rust, source.as_bytes(), "count", RenameTarget::Callable)
                .unwrap();
        let (out, _) = replace_occurrences(source, &found, "tally");
        assert!(out.contains("m.tally)"), "expected the known over-rename:\n{out}");
        assert!(
            out.contains("struct Meter { count: usize }"),
            "the field declaration is outside the macro and must survive:\n{out}"
        );
    }

    #[cfg(feature = "lang-gdscript")]
    #[test]
    fn renaming_a_gdscript_func_leaves_an_identically_named_var_declaration_alone() {
        // The local is declared but never read by name, so nothing here is
        // ambiguous and the rename can proceed.
        let source = "func count():\n\tvar count = 1\n\treturn 2\n\nfunc caller():\n\treturn count()\n";
        let found =
            identifier_occurrences_for(Lang::GdScript, source.as_bytes(), "count", RenameTarget::Callable)
                .unwrap();
        let (out, _) = replace_occurrences(source, &found, "tally");
        assert!(out.contains("func tally():"), "declaration:\n{out}");
        assert!(out.contains("return tally()"), "call:\n{out}");
        assert!(
            out.contains("var count = 1"),
            "the local var declaration was renamed:\n{out}"
        );
    }

    #[cfg(feature = "lang-gdscript")]
    #[test]
    fn a_gdscript_local_that_shadows_the_target_and_is_read_refuses() {
        // Renaming the `func` but not the shadowing `var` while still rewriting
        // `return count` would leave the declaration on the old name and its
        // read on the new one. Refusing names the shadow instead of guessing.
        let source = "func count():\n\tvar count = 1\n\treturn count\n\nfunc caller():\n\treturn count()\n";
        let err = identifier_occurrences_for(
            Lang::GdScript,
            source.as_bytes(),
            "count",
            RenameTarget::Callable,
        )
        .unwrap_err();
        let message = format!("{err:#}");
        assert!(message.contains("shadows it"), "{message}");
        assert!(message.contains("line 2"), "{message}");
    }

    #[cfg(feature = "lang-gdscript")]
    #[test]
    fn a_gdscript_callee_is_never_ambiguous() {
        // A call site is the function whatever else is in scope, so a shadow
        // that is only ever *called* is not a reason to refuse.
        let source = "func count():\n\treturn 1\n\nfunc caller():\n\treturn count() + count()\n";
        let found = identifier_occurrences_for(
            Lang::GdScript,
            source.as_bytes(),
            "count",
            RenameTarget::Callable,
        )
        .unwrap();
        assert_eq!(found.len(), 3, "got {found:?}");
    }

    #[cfg(feature = "lang-gdscript")]
    #[test]
    fn renaming_a_gdscript_var_leaves_the_function_declaration_alone() {
        // The mirror case: the same two `name` nodes, the other target kind.
        let source = "var count = 1\nfunc count():\n\treturn count\n";
        let found =
            identifier_occurrences_for(Lang::GdScript, source.as_bytes(), "count", RenameTarget::Value)
                .unwrap();
        let (out, _) = replace_occurrences(source, &found, "tally");
        assert!(out.contains("var tally = 1"), "var declaration:\n{out}");
        assert!(
            out.contains("func count():"),
            "the function declaration was renamed:\n{out}"
        );
    }

    #[cfg(feature = "lang-gdscript")]
    #[test]
    fn a_gdscript_parameter_is_a_binding_not_a_reference() {
        // The parameter shadows, and `return count` reads it, so this refuses
        // rather than renaming the read out from under the declaration.
        let shadowed = "func caller(count):\n\treturn count\n";
        let err = identifier_occurrences_for(
            Lang::GdScript,
            shadowed.as_bytes(),
            "count",
            RenameTarget::Callable,
        )
        .unwrap_err();
        assert!(format!("{err:#}").contains("shadows it"), "{err:#}");

        // With nothing reading the parameter, the declaration is simply left
        // alone: it is a fresh binding, never a reference to our function.
        let source = "func caller(count):\n\treturn 1\n";
        let found =
            identifier_occurrences_for(Lang::GdScript, source.as_bytes(), "count", RenameTarget::Callable)
                .unwrap();
        let (out, _) = replace_occurrences(source, &found, "tally");
        assert!(
            out.contains("func caller(count):"),
            "a parameter declaration was renamed:\n{out}"
        );
    }

    #[cfg(feature = "lang-typescript")]
    const TS_PROPERTY_SOURCE: &str = r#"function beta(v: number) { return v; }
const keyed = { beta: 1 };
const shorthand = { beta };
class K { beta() { return 2; } }
const k = new K();
const read = k.beta() + keyed.beta + beta(3);
export { beta };
"#;

    #[cfg(feature = "lang-typescript")]
    #[test]
    fn renaming_a_typescript_function_leaves_properties_alone() {
        let found = identifier_occurrences_for(
            Lang::TypeScript,
            TS_PROPERTY_SOURCE.as_bytes(),
            "beta",
            RenameTarget::Callable,
        )
        .unwrap();
        let (out, _) = replace_occurrences(TS_PROPERTY_SOURCE, &found, "gamma");
        // Renamed: the declaration, the call, and the export specifier.
        assert!(out.contains("function gamma(v: number)"), "declaration:
{out}");
        assert!(out.contains("+ gamma(3)"), "call:
{out}");
        assert!(out.contains("export { gamma };"), "export:
{out}");
        // Untouched: every property position.
        assert!(out.contains("{ beta: 1 }"), "object key was renamed:
{out}");
        assert!(
            out.contains("class K { beta()"),
            "class method was renamed:
{out}"
        );
        assert!(out.contains("k.beta()"), "member call was renamed:
{out}");
        assert!(out.contains("keyed.beta"), "member read was renamed:
{out}");
    }

    #[cfg(feature = "lang-typescript")]
    #[test]
    fn a_javascript_object_shorthand_is_expanded_rather_than_overwritten() {
        // `{ beta }` names the property and reads the binding. Overwriting the
        // span would rename the property too; skipping it would leave a read of
        // a name that no longer exists.
        let found = identifier_occurrences_for(
            Lang::TypeScript,
            TS_PROPERTY_SOURCE.as_bytes(),
            "beta",
            RenameTarget::Callable,
        )
        .unwrap();
        let (out, _) = replace_occurrences(TS_PROPERTY_SOURCE, &found, "gamma");
        assert!(
            out.contains("const shorthand = { beta: gamma };"),
            "shorthand was not expanded:
{out}"
        );
    }

    #[cfg(feature = "lang-typescript")]
    #[test]
    fn a_destructuring_pattern_is_renamed_in_place_not_expanded() {
        // `const { beta } = mod` reads a property off `mod`. When `mod` is the
        // module whose export was renamed — the common case — renaming the span
        // is exactly right, and expanding it would be wrong.
        let source = "import * as mod from './mod';
const { beta } = mod;
beta();
";
        let found =
            identifier_occurrences_for(Lang::TypeScript, source.as_bytes(), "beta", RenameTarget::Callable)
                .unwrap();
        let (out, _) = replace_occurrences(source, &found, "gamma");
        assert!(out.contains("const { gamma } = mod;"), "{out}");
        assert!(!out.contains("beta: gamma"), "pattern was expanded:
{out}");
    }

    #[cfg(feature = "lang-typescript")]
    #[test]
    fn a_typescript_type_rename_keeps_type_identifiers_and_drops_properties() {
        let source = "type Beta = number;
const o = { Beta: 1 };
const v: Beta = 1;
export type { Beta };
";
        let callable = identifier_occurrences_for(
            Lang::TypeScript,
            source.as_bytes(),
            "Beta",
            RenameTarget::Callable,
        )
        .unwrap();
        let typed =
            identifier_occurrences_for(Lang::TypeScript, source.as_bytes(), "Beta", RenameTarget::Type)
                .unwrap();
        assert!(
            typed.len() > callable.len(),
            "a type rename must reach type_identifier positions a callable rename does not: {typed:?} vs {callable:?}"
        );
        let (out, _) = replace_occurrences(source, &typed, "Gamma");
        assert!(out.contains("type Gamma = number;"), "{out}");
        assert!(out.contains("const v: Gamma = 1;"), "{out}");
        assert!(out.contains("{ Beta: 1 }"), "object key was renamed:
{out}");
    }

    #[test]
    fn indexed_symbol_kinds_map_onto_what_a_grammar_can_check() {
        assert_eq!(RenameTarget::from_indexed_kind("function"), RenameTarget::Callable);
        assert_eq!(RenameTarget::from_indexed_kind("signal"), RenameTarget::Signal);
        assert_eq!(RenameTarget::from_indexed_kind("struct"), RenameTarget::Type);
        assert_eq!(RenameTarget::from_indexed_kind("class"), RenameTarget::Type);
        assert_eq!(RenameTarget::from_indexed_kind("variable"), RenameTarget::Value);
        assert_eq!(RenameTarget::from_indexed_kind("const"), RenameTarget::Value);
        // An unrecognized kind must be permissive, never silently narrowing.
        assert_eq!(RenameTarget::from_indexed_kind("heading"), RenameTarget::Unresolved);
        assert_eq!(RenameTarget::from_indexed_kind(""), RenameTarget::Unresolved);
        assert_eq!(RenameTarget::default(), RenameTarget::Unresolved);
    }

    #[test]
    fn a_name_that_only_appears_in_prose_has_no_occurrences() {
        #[cfg(feature = "lang-rust")]
        {
            let source = "// widget_count\nfn other() {}\n";
            let found =
                identifier_occurrences(Lang::Rust, source.as_bytes(), "widget_count").unwrap();
            assert!(found.is_empty(), "got {found:?}");
        }
    }

    #[cfg(feature = "lang-markdown")]
    #[test]
    fn markdown_has_no_identifier_kinds() {
        assert!(identifier_node_kinds(Lang::Markdown).is_empty());
        assert!(
            identifier_occurrences(Lang::Markdown, b"# widget_count\n", "widget_count")
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn every_indexed_language_declares_its_identifier_kinds() {
        // A `Lang` variant added with no entry here would silently return an
        // empty set and make every rename in that language a no-op.
        for lang in Lang::all() {
            let kinds = identifier_node_kinds(lang);
            if lang.name() == "markdown" {
                continue;
            }
            assert!(
                !kinds.is_empty(),
                "{} declares no identifier node kinds",
                lang.name()
            );
            let ts_lang = lang.tree_sitter_language();
            for kind in kinds {
                assert!(
                    ts_lang.id_for_node_kind(kind, true) != 0,
                    "{} declares node kind {kind:?}, which its grammar does not have",
                    lang.name()
                );
            }
        }
    }
}
