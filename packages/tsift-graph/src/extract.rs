//! Range-selected function extraction.
//!
//! Every other semantic edit selects a *named* thing — a symbol row, a heading,
//! an ast-grep pattern — and rewrites at or around it. An extraction selects a
//! run of sibling statements that has no name, no symbol row, and no single AST
//! node, and produces two edits that must agree with each other: a new function
//! whose signature is derived from the selection, and a call whose arguments
//! come from the same derivation.
//!
//! That second property is why this module refuses so much. A rename that
//! misses an occurrence breaks the build loudly. An extraction with a wrong
//! parameter list can compile and silently change behaviour — a name that
//! should have been a parameter falling through to a module-level binding of
//! the same name is the exact case, which is why module scope is classified
//! explicitly below rather than left to "not bound in the enclosing function".
//!
//! The derivation itself is language-general; what is not is the *vocabulary*
//! it reads (which node kinds bind, read, block, and escape) and the *spelling*
//! it emits. Those two live in [`Dialect`] and the emitters at the bottom of
//! this file, so a language joins the untyped family by naming its node kinds
//! rather than by growing a second copy of the analysis. The family is exactly
//! the set of languages whose signature is derivable without type information:
//! Python, GDScript, and the JS-like grammars. TypeScript is in it only because
//! it can *copy* an annotation it already has — where it cannot, it refuses
//! rather than writing `unknown` or an implicit `any`.

use crate::lang::Lang;
use std::collections::BTreeSet;
use tree_sitter::{Node, Parser};

/// A derived extraction, ready to be spelled by a language emitter.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExtractionPlan {
    /// The language the plan was derived from; the emitter reads it back so a
    /// plan cannot be rendered with another language's spelling.
    pub lang: Lang,
    /// The function the range was taken out of.
    pub enclosing_function: String,
    /// Names read in the range before the range assigns them, that are bound in
    /// the enclosing function outside the range. Sorted, so the signature and
    /// the call site cannot disagree about argument order.
    pub parameters: Vec<String>,
    /// How each parameter is spelled in the new signature, positionally paired
    /// with `parameters`. Identical to `parameters` in the untyped languages;
    /// in TypeScript each entry carries the annotation copied from the name's
    /// existing binding, because a parameter list is the one place a derived
    /// signature cannot stay silent about types.
    pub parameter_spellings: Vec<String>,
    /// Names assigned in the range and read after it, in the same order rule.
    pub returns: Vec<String>,
    /// The new function's declared return type, where the language requires
    /// one. `None` everywhere the return is inferred.
    pub return_type: Option<String>,
    /// Whether a call site that declares what it receives has to declare it
    /// mutable, because something after the range assigns it.
    pub returns_declared_mut: bool,
    /// Names the range only *assigns*, whose declaration stayed behind in the
    /// enclosing function. In a language where a bare assignment does not
    /// declare, the new function has to declare them itself or its body reads a
    /// name that is not there. Empty in Python, where assignment declares.
    pub local_declarations: Vec<String>,
    /// Whether the call site must *declare* the returned names rather than
    /// assign them: true when the range carried their declaration away with it.
    /// Always false where declarations do not exist (Python).
    pub returns_need_declaration: bool,
    /// Byte range of the statements being hoisted.
    pub start_byte: usize,
    pub end_byte: usize,
    /// Indentation of the hoisted statements, so the emitter can re-indent the
    /// body and place the call at the same depth.
    pub indent: String,
    /// Byte offset where the new function is inserted: immediately after the
    /// enclosing function, at its own indentation.
    pub insert_byte: usize,
    /// Indentation of the enclosing function's own declaration.
    pub enclosing_indent: String,
    /// One level of indentation as this file actually writes it, measured from
    /// the enclosing function's own body rather than assumed. A file indented
    /// with tabs or two spaces gets a new function indented the same way.
    pub indent_unit: String,
}

/// Why an extraction was refused.
///
/// Each variant names one invariant. A refusal is a first-class result here for
/// the same reason it is in `edit-intents`: an extraction that guesses is worse
/// than one that declines.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExtractionRefusal {
    /// The language has no extraction emitter yet.
    UnsupportedLanguage(&'static str),
    /// The source could not be parsed at all.
    ParseFailed,
    /// No statement starts inside the requested line range.
    EmptyRange,
    /// The selected statements do not share one block, so they are not a
    /// contiguous run of siblings.
    NotContiguousSiblings,
    /// The range is not inside a function body.
    NotInsideFunction,
    /// The enclosing function is not itself a statement — it is a method, or a
    /// function expression bound into a larger expression — so there is nowhere
    /// beside it to put a new function without changing what the new function
    /// is.
    EnclosingFunctionNotHoistable,
    /// Control flow leaves the range: hoisting it changes what it does, and no
    /// signature can carry that.
    ControlFlowEscapes(&'static str),
    /// The range assigns a name the enclosing function declared `global` or
    /// `nonlocal`; the assignment's effect is outside the new function's scope.
    RebindsOuterScope(String),
    /// The extracted name already binds something visible at the call site.
    NameCollision(String),
    /// The extraction would have to return several values in a language with no
    /// spelling for that which keeps the call site one statement.
    MultipleReturnsUnsupported(&'static str),
    /// Some returned names were declared inside the range and others already
    /// existed outside it. One call site cannot both declare and assign, and
    /// splitting it into two statements would change what a caller reads.
    MixedReturnDeclarations,
    /// A name the range needs would have to be *moved* into the new function
    /// and is still read afterwards. Passing it by reference instead would
    /// mean rewriting every use in the body into a dereference, which is a
    /// body rewrite this intent does not do.
    MovedNameUsedAfterRange(String),
    /// The range covers the block's trailing expression — the value the
    /// enclosing function returns. Hoisting it would hand that value to the
    /// new function and leave the caller returning nothing.
    ReturnsThroughTailExpression,
    /// The range names the receiver the enclosing function was called on.
    /// Unlike Python's `self`, `this` is not a name a derived signature can
    /// carry, and it means something different inside a plain function.
    ReferencesReceiver(&'static str),
    /// The range assigns a name it does not declare, and that name is not a
    /// local of the enclosing function either. Declaring it inside the new
    /// function would shadow an outer binding or turn a global into a local;
    /// leaving it undeclared would write to a scope the caller did not mean.
    AssignsUndeclaredName(String),
    /// A parameter's type cannot be copied from an existing annotation, and
    /// this language requires one. Writing `unknown` — or leaving it implicitly
    /// `any` — would produce a signature that type-checks and means nothing.
    UnspellableParameterType(String),
}

impl ExtractionRefusal {
    /// A one-line message naming the failed invariant.
    pub fn message(&self) -> String {
        match self {
            Self::UnsupportedLanguage(lang) => {
                format!("extract_function has no emitter for {lang} yet")
            }
            Self::ParseFailed => "source could not be parsed".to_string(),
            Self::EmptyRange => "no statement starts inside the requested line range".to_string(),
            Self::NotContiguousSiblings => {
                "the selected lines are not a contiguous run of sibling statements in one block"
                    .to_string()
            }
            Self::NotInsideFunction => "the selected range is not inside a function".to_string(),
            Self::EnclosingFunctionNotHoistable => {
                "the enclosing function is a method or an expression, so a new function cannot be placed beside it"
                    .to_string()
            }
            Self::ControlFlowEscapes(kind) => {
                format!("the range contains `{kind}`, whose effect leaves the extracted function")
            }
            Self::RebindsOuterScope(name) => {
                format!("the range assigns `{name}`, which the enclosing function declares global or nonlocal")
            }
            Self::NameCollision(name) => {
                format!("`{name}` already binds a value visible at the call site")
            }
            Self::MultipleReturnsUnsupported(lang) => {
                format!("the range produces several values and {lang} has no destructuring call site to receive them")
            }
            Self::MixedReturnDeclarations => {
                "the range produces both newly declared and already declared names, which one call site cannot receive"
                    .to_string()
            }
            Self::MovedNameUsedAfterRange(name) => {
                format!("`{name}` would have to move into the extracted function and is still read after the range")
            }
            Self::ReturnsThroughTailExpression => {
                "the range covers the trailing expression the enclosing function returns".to_string()
            }
            Self::ReferencesReceiver(keyword) => {
                format!("the range uses `{keyword}`, which a derived signature cannot carry out of the method")
            }
            Self::AssignsUndeclaredName(name) => {
                format!("the range assigns `{name}` without declaring it, and `{name}` is not a local of the enclosing function")
            }
            Self::UnspellableParameterType(name) => {
                format!("`{name}` has no annotation to copy, and this language will not spell a type it cannot see")
            }
        }
    }
}

/// The node-kind vocabulary and spelling rules of one extractable language.
///
/// Data, not prose: a grammar that renames `block` to `body` shows up as a
/// changed row rather than as a comment that quietly stopped being true.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Dialect {
    family: Family,
    /// Whether the new signature has to carry parameter types.
    annotates_parameters: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Family {
    /// `def`, indentation blocks, no declarations, tuple returns.
    Python,
    /// `func`, indentation blocks, `var` declarations, no destructuring.
    GdScript,
    /// `function`, brace blocks, `let` declarations, array destructuring.
    JsLike,
    /// `fn`, brace blocks, `let` declarations, tuple returns — and the only
    /// member of the family whose signature carries ownership as well as a
    /// type. See `rust_move_only` for why that keeps it by-value.
    Rust,
}

fn dialect_for(lang: Lang) -> Option<Dialect> {
    let dialect = match lang {
        #[cfg(feature = "lang-python")]
        Lang::Python => Dialect {
            family: Family::Python,
            annotates_parameters: false,
        },
        #[cfg(feature = "lang-gdscript")]
        Lang::GdScript => Dialect {
            family: Family::GdScript,
            annotates_parameters: false,
        },
        #[cfg(feature = "lang-javascript")]
        Lang::JavaScript | Lang::Jsx => Dialect {
            family: Family::JsLike,
            annotates_parameters: false,
        },
        #[cfg(feature = "lang-typescript")]
        Lang::TypeScript | Lang::Tsx => Dialect {
            family: Family::JsLike,
            annotates_parameters: true,
        },
        #[cfg(feature = "lang-rust")]
        Lang::Rust => Dialect {
            family: Family::Rust,
            annotates_parameters: true,
        },
        _ => return None,
    };
    Some(dialect)
}

impl Dialect {
    fn root_kind(self) -> &'static str {
        match self.family {
            Family::Python => "module",
            Family::GdScript => "source",
            Family::JsLike => "program",
            Family::Rust => "source_file",
        }
    }

    fn is_block_kind(self, kind: &str) -> bool {
        match self.family {
            Family::Python => kind == "block",
            Family::GdScript => kind == "body",
            Family::JsLike => kind == "statement_block",
            Family::Rust => kind == "block",
        }
    }

    fn is_function_kind(self, kind: &str) -> bool {
        match self.family {
            Family::Python | Family::GdScript => kind == "function_definition",
            Family::JsLike => matches!(
                kind,
                "function_declaration"
                    | "generator_function_declaration"
                    | "function_expression"
                    | "function"
                    | "generator_function"
                    | "arrow_function"
                    | "method_definition"
            ),
            // `closure_expression` is listed so a range inside a closure
            // resolves to the closure rather than to the `fn` around it, and
            // then refuses at the insertion site — hoisting past a closure
            // would strand everything it captured.
            Family::Rust => matches!(kind, "function_item" | "closure_expression"),
        }
    }

    /// A scope that re-binds names of its own, so its body is not part of the
    /// range's control flow or of the enclosing function's bindings.
    fn is_nested_scope_kind(self, kind: &str) -> bool {
        self.is_function_kind(kind)
            || match self.family {
                Family::Python => matches!(kind, "lambda" | "class_definition"),
                Family::GdScript => matches!(kind, "lambda" | "class_definition"),
                Family::JsLike => matches!(kind, "class_declaration" | "class"),
                Family::Rust => matches!(
                    kind,
                    "impl_item" | "trait_item" | "struct_item" | "enum_item" | "mod_item"
                ),
            }
    }

    fn is_class_kind(self, kind: &str) -> bool {
        match self.family {
            Family::Python | Family::GdScript => kind == "class_definition",
            Family::JsLike => matches!(kind, "class_declaration" | "class"),
            Family::Rust => matches!(kind, "impl_item" | "trait_item"),
        }
    }

    /// The node that holds a class's members, where it is spelled separately
    /// from the class itself.
    fn is_class_body_kind(self, kind: &str) -> bool {
        match self.family {
            // Python reuses its ordinary block node, so a Python class body is
            // recognized through its parent instead.
            Family::Python => false,
            Family::GdScript | Family::JsLike => kind == "class_body",
            Family::Rust => kind == "declaration_list",
        }
    }

    /// Whether a new function extracted from a method belongs *outside* the
    /// class rather than beside the method.
    ///
    /// Python and JavaScript resolve a bare call through the enclosing lexical
    /// scope, never through the class, so the new function has to leave.
    /// GDScript resolves a bare call against the script's own members, so it
    /// has to stay.
    fn hoists_out_of_class(self) -> bool {
        !matches!(self.family, Family::GdScript)
    }

    /// Node kinds that name the receiver the enclosing function was called on.
    ///
    /// A `this` moved into a plain function stops meaning what it meant, and
    /// unlike Python's `self` it is not a name a signature can carry.
    fn receiver_kinds(self) -> &'static [&'static str] {
        match self.family {
            Family::Python | Family::GdScript => &[],
            Family::JsLike => &["this", "super"],
            Family::Rust => &["self"],
        }
    }

    /// Kinds whose effect is defined by something outside themselves.
    ///
    /// `break` and `continue` are conditional — see `escaping_control_flow`,
    /// which only counts them when the loop they bind to is outside the range.
    fn escaping_kind(self, kind: &str) -> Option<&'static str> {
        let escape = match (self.family, kind) {
            (_, "return_statement") => "return",
            (_, "break_statement") => "break",
            (_, "continue_statement") => "continue",
            (Family::Python, "yield") => "yield",
            (Family::JsLike, "yield_expression") => "yield",
            (Family::Rust, "return_expression") => "return",
            (Family::Rust, "break_expression") => "break",
            (Family::Rust, "continue_expression") => "continue",
            // `?` returns from the *enclosing* function, and `.await` needs a
            // context the new function's signature does not say it has.
            (Family::Rust, "try_expression") => "?",
            (Family::Rust, "await_expression") => ".await",
            // `throw`/`raise` is deliberately absent: an exception propagates
            // through a call frame unchanged, so hoisting it does not move
            // where it is caught.
            _ => return None,
        };
        Some(escape)
    }

    /// A construct `break` and `continue` bind to.
    fn is_loop_kind(self, kind: &str) -> bool {
        match self.family {
            Family::Python | Family::GdScript => matches!(kind, "for_statement" | "while_statement"),
            Family::JsLike => matches!(
                kind,
                "for_statement" | "for_in_statement" | "while_statement" | "do_statement"
            ),
            Family::Rust => {
                matches!(kind, "for_expression" | "while_expression" | "loop_expression")
            }
        }
    }

    /// A construct `break` alone binds to.
    fn is_switch_kind(self, kind: &str) -> bool {
        matches!(self.family, Family::JsLike) && kind == "switch_statement"
    }

    /// Kinds that group several binding positions into one target.
    fn pattern_kinds(self) -> &'static [&'static str] {
        match self.family {
            Family::Python => &["pattern_list", "tuple_pattern", "list_pattern"],
            Family::GdScript => &[],
            Family::Rust => &[
                "tuple_pattern",
                "tuple_struct_pattern",
                "struct_pattern",
                "slice_pattern",
                "ref_pattern",
                "mut_pattern",
            ],
            Family::JsLike => &[
                "object_pattern",
                "array_pattern",
                "pair_pattern",
                "rest_pattern",
                "assignment_pattern",
                "object_assignment_pattern",
            ],
        }
    }

    /// The keyword a call site uses to declare the names it receives, where the
    /// language has one.
    fn declaration_keyword(self) -> Option<&'static str> {
        match self.family {
            Family::Python => None,
            Family::GdScript => Some("var"),
            Family::JsLike | Family::Rust => Some("let"),
        }
    }

    fn default_indent_unit(self) -> &'static str {
        match self.family {
            Family::Python | Family::Rust => "    ",
            Family::GdScript => "\t",
            Family::JsLike => "  ",
        }
    }

    /// Whether a parameter is moved rather than borrowed, so a name handed in
    /// is gone from the caller unless it is handed back.
    fn moves_parameters(self) -> bool {
        matches!(self.family, Family::Rust)
    }

    /// Whether the signature spells mutability as well as a type.
    fn annotates_mutability(self) -> bool {
        matches!(self.family, Family::Rust)
    }

    /// Whether the language requires the new function to declare its return
    /// type rather than inferring it.
    fn annotates_return_type(self) -> bool {
        matches!(self.family, Family::Rust)
    }

    /// How many values a single call-site statement can receive.
    fn max_returns(self) -> Option<usize> {
        match self.family {
            // GDScript has no multiple assignment and no destructuring, so more
            // than one returned name cannot be received without splitting the
            // call site into statements a caller did not ask for.
            Family::GdScript => Some(1),
            _ => None,
        }
    }
}

/// Derive an extraction for the statements covered by `start_line..=end_line`
/// (zero-based, inclusive).
pub fn plan_extraction(
    lang: Lang,
    source: &[u8],
    start_line: usize,
    end_line: usize,
    new_name: &str,
) -> Result<ExtractionPlan, ExtractionRefusal> {
    let dialect =
        dialect_for(lang).ok_or(ExtractionRefusal::UnsupportedLanguage(lang.name()))?;
    let ts_lang = lang.tree_sitter_language();
    let mut parser = Parser::new();
    parser
        .set_language(&ts_lang)
        .map_err(|_| ExtractionRefusal::ParseFailed)?;
    let tree = parser
        .parse(source, None)
        .ok_or(ExtractionRefusal::ParseFailed)?;
    let root = tree.root_node();

    let selection = select_sibling_run(dialect, root, start_line, end_line)?;
    let block = selection[0]
        .parent()
        .ok_or(ExtractionRefusal::NotContiguousSiblings)?;
    let function = enclosing_function(dialect, block)?;
    let insertion_site = insertion_site(dialect, function)?;

    for statement in &selection {
        if is_tail_expression(dialect, *statement) {
            return Err(ExtractionRefusal::ReturnsThroughTailExpression);
        }
        if let Some(kind) = escaping_control_flow(dialect, *statement, source) {
            return Err(ExtractionRefusal::ControlFlowEscapes(kind));
        }
        if let Some(kind) = receiver_reference(dialect, *statement) {
            return Err(ExtractionRefusal::ReferencesReceiver(kind));
        }
    }

    let start_byte = selection[0].start_byte();
    let end_byte = selection[selection.len() - 1].end_byte();

    let bindings = range_bindings(dialect, &selection, source);
    let assigned_in_range = bindings.all();
    let scope_pinned = scope_pinned_names(dialect, function, source);
    if let Some(name) = assigned_in_range.intersection(&scope_pinned).next() {
        return Err(ExtractionRefusal::RebindsOuterScope(name.clone()));
    }

    let read_first_in_range = names_read_before_assignment(dialect, &selection, source);
    let bound_outside_range =
        names_bound_in_function_outside(dialect, function, start_byte, end_byte, source);
    let read_after_range = names_read_after(dialect, function, end_byte, source);
    let module_scope = scope_bindings(dialect, root, source);
    // Where the new function actually lands, which is not module scope once it
    // has climbed out of a class — or into a GDScript class alongside methods
    // the root walk never sees.
    let sibling_scope = insertion_site
        .parent()
        .map(|parent| scope_bindings(dialect, parent, source))
        .unwrap_or_default();

    if bound_outside_range.contains(new_name)
        || module_scope.contains(new_name)
        || sibling_scope.contains(new_name)
    {
        return Err(ExtractionRefusal::NameCollision(new_name.to_string()));
    }

    let mut parameters = read_first_in_range
        .intersection(&bound_outside_range)
        .cloned()
        .collect::<Vec<_>>();
    // A receiver threaded out of a method reads as the first argument
    // everywhere else in the language; alphabetical order would put it in the
    // middle and make a correct signature look wrong.
    if let Some(position) = parameters.iter().position(|name| name == "self") {
        let receiver = parameters.remove(position);
        parameters.insert(0, receiver);
    }
    let returns = assigned_in_range
        .intersection(&read_after_range)
        .cloned()
        .collect::<Vec<_>>();

    if let Some(limit) = dialect.max_returns()
        && returns.len() > limit
    {
        return Err(ExtractionRefusal::MultipleReturnsUnsupported(lang.name()));
    }

    let returns_need_declaration = resolve_return_declaration(
        dialect,
        &returns,
        &bindings.declared,
        &bound_outside_range,
    )?;
    let local_declarations =
        resolve_local_declarations(dialect, &bindings, &parameters, &bound_outside_range)?;

    // Rust passes every parameter by value, so a name that is moved in and not
    // handed back cannot still be read afterwards. Passing it by reference
    // would compile only after rewriting each use in the body into a
    // dereference, and rewriting bodies is the one thing this intent does not
    // do — so it refuses and says which name forced it.
    if dialect.moves_parameters() {
        for parameter in &parameters {
            if read_after_range.contains(parameter) && !returns.contains(parameter) {
                return Err(ExtractionRefusal::MovedNameUsedAfterRange(parameter.clone()));
            }
        }
    }

    let mut parameter_spellings = Vec::with_capacity(parameters.len());
    for parameter in &parameters {
        let mutable = dialect.annotates_mutability() && bindings.all().contains(parameter);
        parameter_spellings.push(spell_parameter(
            dialect, function, parameter, mutable, source,
        )?);
    }
    let return_type = spell_return_type(dialect, function, &returns, source)?;
    let returns_declared_mut = returns_need_declaration
        && returns
            .iter()
            .any(|name| names_assigned_after(dialect, function, end_byte, source).contains(name));

    Ok(ExtractionPlan {
        lang,
        enclosing_function: function
            .child_by_field_name("name")
            .and_then(|name| name.utf8_text(source).ok())
            .unwrap_or_default()
            .to_string(),
        parameters,
        parameter_spellings,
        returns,
        return_type,
        returns_declared_mut,
        local_declarations,
        returns_need_declaration,
        start_byte,
        end_byte,
        indent: line_indent(source, start_byte),
        insert_byte: insertion_site.end_byte(),
        enclosing_indent: line_indent(source, insertion_site.start_byte()),
        indent_unit: indent_unit(dialect, function, source),
    })
}

/// Render an extraction: the new function and the call that replaces the range,
/// both already indented for their positions.
pub fn render_extraction(plan: &ExtractionPlan, source: &str, new_name: &str) -> (String, String) {
    let dialect = dialect_for(plan.lang).expect("a plan is only built for an extractable language");
    let inner_indent = format!("{}{}", plan.enclosing_indent, plan.indent_unit);
    let body = format!(
        "{}{}",
        local_declaration_prologue(&dialect, plan, &inner_indent),
        reindent_body(plan, source, &inner_indent)
    );
    let signature = plan.parameter_spellings.join(", ");
    let arguments = plan.parameters.join(", ");
    let call_expression = format!("{new_name}({arguments})");

    match dialect.family {
        Family::Python => {
            let mut function = format!(
                "\n\n{}def {new_name}({signature}):\n{body}",
                plan.enclosing_indent
            );
            if !plan.returns.is_empty() {
                function.push('\n');
                function.push_str(&inner_indent);
                function.push_str("return ");
                function.push_str(&plan.returns.join(", "));
            }
            function.push('\n');
            let call = if plan.returns.is_empty() {
                format!("{}{call_expression}", plan.indent)
            } else {
                format!(
                    "{}{} = {call_expression}",
                    plan.indent,
                    plan.returns.join(", ")
                )
            };
            (function, call)
        }
        Family::GdScript => {
            let mut function = format!(
                "\n\n{}func {new_name}({signature}):\n{body}",
                plan.enclosing_indent
            );
            if let Some(returned) = plan.returns.first() {
                function.push('\n');
                function.push_str(&inner_indent);
                function.push_str("return ");
                function.push_str(returned);
            }
            function.push('\n');
            let call = match plan.returns.first() {
                None => format!("{}{call_expression}", plan.indent),
                Some(returned) => format!(
                    "{}{}{returned} = {call_expression}",
                    plan.indent,
                    declaration_prefix(&dialect, plan)
                ),
            };
            (function, call)
        }
        Family::JsLike => {
            let mut function = format!(
                "\n\n{}function {new_name}({signature}) {{\n{body}",
                plan.enclosing_indent
            );
            if !plan.returns.is_empty() {
                function.push('\n');
                function.push_str(&inner_indent);
                function.push_str("return ");
                function.push_str(&js_return_target(&plan.returns));
                function.push(';');
            }
            function.push('\n');
            function.push_str(&plan.enclosing_indent);
            function.push_str("}\n");
            let call = if plan.returns.is_empty() {
                format!("{}{call_expression};", plan.indent)
            } else {
                format!(
                    "{}{}{} = {call_expression};",
                    plan.indent,
                    declaration_prefix(&dialect, plan),
                    js_return_target(&plan.returns)
                )
            };
            (function, call)
        }
        Family::Rust => {
            let returns = match &plan.return_type {
                Some(spelled) => format!(" -> {spelled}"),
                None => String::new(),
            };
            let mut function = format!(
                "\n\n{}fn {new_name}({signature}){returns} {{\n{body}",
                plan.enclosing_indent
            );
            if !plan.returns.is_empty() {
                // A trailing expression, not `return`: the idiom the language
                // reads as a value handed back rather than a jump.
                function.push('\n');
                function.push_str(&inner_indent);
                function.push_str(&rust_return_target(&plan.returns));
            }
            function.push('\n');
            function.push_str(&plan.enclosing_indent);
            function.push_str("}\n");
            let call = if plan.returns.is_empty() {
                format!("{}{call_expression};", plan.indent)
            } else {
                format!(
                    "{}{}{} = {call_expression};",
                    plan.indent,
                    declaration_prefix(&dialect, plan),
                    rust_return_target(&plan.returns)
                )
            };
            (function, call)
        }
    }
}

/// One returned name is itself; several are a tuple, which the call site
/// destructures in the same shape.
fn rust_return_target(returns: &[String]) -> String {
    if returns.len() == 1 {
        returns[0].clone()
    } else {
        format!("({})", returns.join(", "))
    }
}

/// The declarations the new function opens with, for names the range assigns
/// but whose declaration stayed behind in the enclosing function.
fn local_declaration_prologue(
    dialect: &Dialect,
    plan: &ExtractionPlan,
    inner_indent: &str,
) -> String {
    let Some(keyword) = dialect.declaration_keyword() else {
        return String::new();
    };
    let terminator = if matches!(dialect.family, Family::JsLike | Family::Rust) {
        ";"
    } else {
        ""
    };
    plan.local_declarations
        .iter()
        .map(|name| format!("{inner_indent}{keyword} {name}{terminator}\n"))
        .collect()
}

/// `let ` / `var ` when the range carried the declaration away, empty when the
/// names still exist at the call site.
fn declaration_prefix(dialect: &Dialect, plan: &ExtractionPlan) -> String {
    if !plan.returns_need_declaration {
        return String::new();
    }
    let Some(keyword) = dialect.declaration_keyword() else {
        return String::new();
    };
    // Only where the binding spells mutability, and only when something after
    // the range assigns it — an unconditional `mut` would compile and warn.
    let mutable = if dialect.annotates_mutability() && plan.returns_declared_mut {
        "mut "
    } else {
        ""
    };
    format!("{keyword} {mutable}")
}

/// One returned name is itself; several are an array, which is the only
/// spelling that keeps the JS call site a single statement.
fn js_return_target(returns: &[String]) -> String {
    if returns.len() == 1 {
        returns[0].clone()
    } else {
        format!("[{}]", returns.join(", "))
    }
}

/// The hoisted statements, re-indented for the new function's body while
/// keeping their relative nesting.
fn reindent_body(plan: &ExtractionPlan, source: &str, inner_indent: &str) -> String {
    let body = &source[plan.start_byte..plan.end_byte];
    let mut rendered = String::new();
    for (index, line) in body.lines().enumerate() {
        if index > 0 {
            rendered.push('\n');
        }
        if line.trim().is_empty() {
            continue;
        }
        let stripped = line.strip_prefix(&plan.indent).unwrap_or(line);
        rendered.push_str(inner_indent);
        rendered.push_str(stripped);
    }
    rendered
}

/// Whether the call site declares the names it receives.
///
/// A name needs declaring only when the range carried its *declaration* away
/// and nothing outside the range binds it. Reading declarations rather than
/// bindings is what keeps an implicit global — assigned in the range, never
/// declared anywhere — from being turned into a local by the call site.
///
/// Mixed is a refusal rather than a guess: a call site that declared the new
/// names and assigned the old ones would need two statements, and the second
/// would read a binding the first had just shadowed.
fn resolve_return_declaration(
    dialect: Dialect,
    returns: &[String],
    declared_in_range: &BTreeSet<String>,
    bound_outside_range: &BTreeSet<String>,
) -> Result<bool, ExtractionRefusal> {
    if dialect.declaration_keyword().is_none() || returns.is_empty() {
        return Ok(false);
    }
    let new_names = returns
        .iter()
        .filter(|name| {
            declared_in_range.contains(*name) && !bound_outside_range.contains(*name)
        })
        .count();
    if new_names == 0 {
        return Ok(false);
    }
    if new_names == returns.len() {
        return Ok(true);
    }
    Err(ExtractionRefusal::MixedReturnDeclarations)
}

/// How one parameter is written in the new signature.
///
/// Only TypeScript needs more than the name, and it gets it by *copying* an
/// annotation the file already has rather than inventing one.
fn spell_parameter(
    dialect: Dialect,
    function: Node,
    name: &str,
    mutable: bool,
    source: &[u8],
) -> Result<String, ExtractionRefusal> {
    if !dialect.annotates_parameters {
        return Ok(name.to_string());
    }
    let annotation = existing_type_annotation(dialect, function, name, source)
        .ok_or_else(|| ExtractionRefusal::UnspellableParameterType(name.to_string()))?;
    Ok(match dialect.family {
        // TypeScript's annotation node carries its own `: `.
        Family::Rust => {
            let prefix = if mutable { "mut " } else { "" };
            format!("{prefix}{name}: {annotation}")
        }
        _ => format!("{name}{annotation}"),
    })
}

/// The type the new function declares it returns, where the language makes it
/// say so. Several returns are a tuple, which is also the shape the call site
/// destructures.
fn spell_return_type(
    dialect: Dialect,
    function: Node,
    returns: &[String],
    source: &[u8],
) -> Result<Option<String>, ExtractionRefusal> {
    if !dialect.annotates_return_type() || returns.is_empty() {
        return Ok(None);
    }
    let mut spelled = Vec::with_capacity(returns.len());
    for name in returns {
        spelled.push(
            existing_type_annotation(dialect, function, name, source)
                .ok_or_else(|| ExtractionRefusal::UnspellableParameterType(name.clone()))?,
        );
    }
    Ok(Some(if spelled.len() == 1 {
        spelled.remove(0)
    } else {
        format!("({})", spelled.join(", "))
    }))
}

/// The annotation text (`": number"`) attached to `name`'s binding inside the
/// enclosing function, if it has one.
fn existing_type_annotation(
    dialect: Dialect,
    function: Node,
    name: &str,
    source: &[u8],
) -> Option<String> {
    let mut found = None;
    walk(function, &mut |node| {
        if found.is_some() {
            return false;
        }
        let binder = match (dialect.family, node.kind()) {
            (Family::JsLike, "required_parameter" | "optional_parameter") => {
                node.child_by_field_name("pattern")
            }
            (Family::JsLike, "variable_declarator") => node.child_by_field_name("name"),
            (Family::Rust, "parameter" | "let_declaration") => {
                node.child_by_field_name("pattern")
            }
            _ => None,
        };
        if let Some(binder) = binder
            && binder.kind() == "identifier"
            && binder.utf8_text(source).is_ok_and(|text| text == name)
            && let Some(annotation) = node.child_by_field_name("type")
            && let Ok(text) = annotation.utf8_text(source)
        {
            found = Some(text.to_string());
            return false;
        }
        true
    });
    found
}

/// One level of indentation as the enclosing function's own body writes it.
fn indent_unit(dialect: Dialect, function: Node, source: &[u8]) -> String {
    let enclosing_indent = line_indent(source, function.start_byte());
    let measured = function
        .child_by_field_name("body")
        .and_then(|body| body.named_child(0))
        .map(|statement| line_indent(source, statement.start_byte()))
        .and_then(|body_indent| {
            body_indent
                .strip_prefix(&enclosing_indent)
                .map(str::to_string)
        })
        .filter(|unit| !unit.is_empty());
    measured.unwrap_or_else(|| dialect.default_indent_unit().to_string())
}

/// The statements that start inside the requested lines, verified to be a
/// contiguous run of siblings in one block.
fn select_sibling_run(
    dialect: Dialect,
    root: Node,
    start_line: usize,
    end_line: usize,
) -> Result<Vec<Node>, ExtractionRefusal> {
    let mut selected: Vec<Node> = Vec::new();
    let mut cursor = root.walk();
    let mut descend = true;
    loop {
        if descend {
            let node = cursor.node();
            let row = node.start_position().row;
            if node.is_named()
                && row >= start_line
                && row <= end_line
                && node.parent().is_some_and(|parent| {
                    dialect.is_block_kind(parent.kind()) || parent.kind() == dialect.root_kind()
                })
            {
                selected.push(node);
                // A statement's children cannot also be top-level statements of
                // the same run, so the walk does not descend into a match.
                if cursor.goto_next_sibling() {
                    continue;
                }
                if !cursor.goto_parent() {
                    break;
                }
                descend = false;
                continue;
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

    if selected.is_empty() {
        return Err(ExtractionRefusal::EmptyRange);
    }
    let first_parent = selected[0].parent().map(|parent| parent.id());
    if selected
        .iter()
        .any(|node| node.parent().map(|parent| parent.id()) != first_parent)
    {
        return Err(ExtractionRefusal::NotContiguousSiblings);
    }
    // Siblings in source order with nothing named between them.
    for pair in selected.windows(2) {
        if pair[0].next_named_sibling().map(|next| next.id()) != Some(pair[1].id()) {
            return Err(ExtractionRefusal::NotContiguousSiblings);
        }
    }
    Ok(selected)
}

/// The enclosing function, verified to be a statement something can be placed
/// beside.
///
/// A method or a function expression fails here rather than later: hoisting out
/// of a method would emit a sibling method, and the bare call left behind would
/// not resolve to it — code that parses, formats, and does not run.
fn enclosing_function(dialect: Dialect, block: Node) -> Result<Node, ExtractionRefusal> {
    let mut current = Some(block);
    while let Some(candidate) = current {
        if dialect.is_function_kind(candidate.kind()) {
            return Ok(candidate);
        }
        current = candidate.parent();
    }
    Err(ExtractionRefusal::NotInsideFunction)
}

/// The construct the new function is placed after.
///
/// Usually the enclosing function itself. Inside a method it is the *class*,
/// because a `def` placed beside a method is another method and the bare call
/// left behind does not resolve to it — so the extraction climbs out to where
/// the call can see it. Climbing past a class body never costs the extracted
/// body anything: a method could not read a class-body name unqualified in the
/// first place, so nothing it closed over is left behind.
///
/// GDScript is the exception, and for the opposite reason: its methods *do*
/// call each other bare, so a sibling `func` in the same class is exactly
/// right and climbing out would break the call instead of fixing it.
fn insertion_site(dialect: Dialect, function: Node) -> Result<Node, ExtractionRefusal> {
    let mut node = function;
    loop {
        let Some(parent) = node.parent() else {
            return Err(ExtractionRefusal::EnclosingFunctionNotHoistable);
        };
        // A class body holds declarations the same way a block holds
        // statements, so both are places something can be put; what differs is
        // whether staying there keeps the call resolvable.
        let in_class_body = dialect.is_class_body_kind(parent.kind())
            || (dialect.is_block_kind(parent.kind())
                && parent
                    .parent()
                    .is_some_and(|grand| dialect.is_class_kind(grand.kind())));
        if in_class_body {
            if !dialect.hoists_out_of_class() {
                return Ok(node);
            }
            let Some(class) = parent.parent() else {
                return Err(ExtractionRefusal::EnclosingFunctionNotHoistable);
            };
            node = class;
            continue;
        }
        if dialect.is_block_kind(parent.kind()) || parent.kind() == dialect.root_kind() {
            return Ok(node);
        }
        // Everything else — an arrow function, a function expression, a class
        // expression — is part of a larger expression, and there is no
        // statement slot beside it to put anything in.
        return Err(ExtractionRefusal::EnclosingFunctionNotHoistable);
    }
}

/// Control flow whose effect is defined outside the range, and which therefore
/// cannot move into another function.
///
/// `return` and `yield` always qualify: no signature can carry them. `break`
/// and `continue` only qualify when the loop they bind to is *outside* the
/// selection — a range containing a whole loop takes that loop's `break` with
/// it, and refusing there would decline the most ordinary extraction there is.
/// A labelled branch is checked against the labels the range itself carries.
fn escaping_control_flow(
    dialect: Dialect,
    statement: Node,
    source: &[u8],
) -> Option<&'static str> {
    let mut labels = Vec::new();
    scan_control_flow(dialect, statement, source, true, 0, 0, &mut labels)
}

fn scan_control_flow(
    dialect: Dialect,
    node: Node,
    source: &[u8],
    is_root: bool,
    loops: usize,
    switches: usize,
    labels: &mut Vec<String>,
) -> Option<&'static str> {
    // A nested function or lambda re-scopes these, so its body is not part of
    // the range's control flow.
    if !is_root && dialect.is_nested_scope_kind(node.kind()) {
        return None;
    }
    match dialect.escaping_kind(node.kind()) {
        Some(escape @ ("break" | "continue")) => {
            let bound_here = match escape {
                "break" => loops > 0 || switches > 0,
                _ => loops > 0,
            };
            return match branch_label(node, source) {
                // A labelled branch ignores the innermost loop and jumps to
                // the label, so what matters is whether the label is inside
                // the range.
                Some(label) if !labels.contains(&label) => Some(escape),
                Some(_) => None,
                None if bound_here => None,
                None => Some(escape),
            };
        }
        Some(escape) => return Some(escape),
        None => {}
    }

    let loops = loops + usize::from(dialect.is_loop_kind(node.kind()));
    let switches = switches + usize::from(dialect.is_switch_kind(node.kind()));
    let pushed = label_name(node, source).inspect(|label| labels.push(label.clone()));

    let mut found = None;
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        found = scan_control_flow(dialect, child, source, false, loops, switches, labels);
        if found.is_some() {
            break;
        }
    }
    if pushed.is_some() {
        labels.pop();
    }
    found
}

/// The label a `break`/`continue` names, where the language has them.
fn branch_label(node: Node, source: &[u8]) -> Option<String> {
    node.child_by_field_name("label")
        .and_then(|label| label.utf8_text(source).ok())
        .map(str::to_string)
}

/// The label this statement defines, if it defines one.
fn label_name(node: Node, source: &[u8]) -> Option<String> {
    if node.kind() != "labeled_statement" {
        return None;
    }
    branch_label(node, source)
}

/// Whether this node is the block's trailing expression rather than a
/// statement — the value its function hands back.
///
/// Only Rust has one. It is recognized structurally: a block's children are
/// statements plus an optional final expression, so a last child that is not a
/// statement kind is that expression.
fn is_tail_expression(dialect: Dialect, node: Node) -> bool {
    if !matches!(dialect.family, Family::Rust) {
        return false;
    }
    if node.next_named_sibling().is_some() {
        return false;
    }
    if !node
        .parent()
        .is_some_and(|parent| dialect.is_block_kind(parent.kind()))
    {
        return false;
    }
    !matches!(node.kind(), "expression_statement" | "let_declaration")
        && !node.kind().ends_with("_item")
        && node.kind() != "attribute_item"
        && node.kind() != "macro_invocation"
}

/// Names something after the range assigns, so a call site that declares what
/// it receives knows whether to declare it mutable.
fn names_assigned_after(
    dialect: Dialect,
    function: Node,
    end_byte: usize,
    source: &[u8],
) -> BTreeSet<String> {
    let mut names = BTreeSet::new();
    walk(function, &mut |node| {
        if node.end_byte() <= end_byte {
            return false;
        }
        if node.start_byte() >= end_byte
            && node
                .parent()
                .is_some_and(|parent| is_assignment_kind(dialect, parent.kind()))
            && let Some(name) = binding_name(dialect, node, source)
        {
            names.insert(name);
        }
        true
    });
    names
}

/// The receiver keyword the range names, if it names one.
fn receiver_reference(dialect: Dialect, statement: Node) -> Option<&'static str> {
    let keywords = dialect.receiver_kinds();
    if keywords.is_empty() {
        return None;
    }
    let mut found = None;
    walk(statement, &mut |node| {
        if found.is_some() {
            return false;
        }
        found = keywords
            .iter()
            .find(|keyword| **keyword == node.kind())
            .copied();
        found.is_none()
    });
    found
}

/// Names the enclosing function pinned to an outer scope.
fn scope_pinned_names(dialect: Dialect, function: Node, source: &[u8]) -> BTreeSet<String> {
    let mut names = BTreeSet::new();
    if dialect.family != Family::Python {
        return names;
    }
    walk(function, &mut |node| {
        if matches!(node.kind(), "global_statement" | "nonlocal_statement") {
            let mut cursor = node.walk();
            for child in node.named_children(&mut cursor) {
                if child.kind() == "identifier"
                    && let Ok(text) = child.utf8_text(source)
                {
                    names.insert(text.to_string());
                }
            }
        }
        true
    });
    names
}

/// What the range does to each name it binds.
///
/// The split matters only in languages where a bare assignment does not
/// declare: a `let` that moved into the new function takes its declaration
/// with it, while an assignment to a name declared behind leaves the new
/// function reading something that is not there.
struct RangeBindings {
    /// Names bound by a construct that declares: a declaration statement, a
    /// loop head, a catch parameter, a nested function or class.
    declared: BTreeSet<String>,
    /// Names bound only by a plain or augmented assignment.
    assigned: BTreeSet<String>,
}

impl RangeBindings {
    fn all(&self) -> BTreeSet<String> {
        self.declared.union(&self.assigned).cloned().collect()
    }
}

fn range_bindings(dialect: Dialect, statements: &[Node], source: &[u8]) -> RangeBindings {
    let mut declared = BTreeSet::new();
    let mut assigned = BTreeSet::new();
    for statement in statements {
        walk(*statement, &mut |node| {
            if let Some(name) = binding_name(dialect, node, source) {
                if node
                    .parent()
                    .is_some_and(|parent| is_assignment_kind(dialect, parent.kind()))
                {
                    assigned.insert(name);
                } else {
                    declared.insert(name);
                }
            }
            true
        });
    }
    // A name the range both declares and assigns is declared: the declaration
    // moved with the statements.
    assigned.retain(|name| !declared.contains(name));
    RangeBindings { declared, assigned }
}

/// The names the new function has to declare for its own body to make sense.
///
/// Only a name the range *assigns* without declaring needs one, and only where
/// a bare assignment does not declare. A name that is not a local of the
/// enclosing function refuses instead: declaring it would shadow an outer
/// binding, and not declaring it would write to a scope the caller did not
/// choose. Python is exempt by construction — there, assignment declares.
fn resolve_local_declarations(
    dialect: Dialect,
    bindings: &RangeBindings,
    parameters: &[String],
    bound_outside_range: &BTreeSet<String>,
) -> Result<Vec<String>, ExtractionRefusal> {
    if dialect.declaration_keyword().is_none() {
        return Ok(Vec::new());
    }
    let mut locals = Vec::new();
    for name in &bindings.assigned {
        // A parameter is already declared by the signature.
        if parameters.iter().any(|parameter| parameter == name) {
            continue;
        }
        if !bound_outside_range.contains(name) {
            return Err(ExtractionRefusal::AssignsUndeclaredName(name.clone()));
        }
        locals.push(name.clone());
    }
    Ok(locals)
}

fn is_assignment_kind(dialect: Dialect, kind: &str) -> bool {
    match dialect.family {
        Family::Python | Family::GdScript => matches!(kind, "assignment" | "augmented_assignment"),
        Family::JsLike => matches!(
            kind,
            "assignment_expression" | "augmented_assignment_expression"
        ),
        Family::Rust => matches!(kind, "assignment_expression" | "compound_assignment_expr"),
    }
}

fn names_bound_in_function_outside(
    dialect: Dialect,
    function: Node,
    start_byte: usize,
    end_byte: usize,
    source: &[u8],
) -> BTreeSet<String> {
    let mut names = BTreeSet::new();
    walk(function, &mut |node| {
        if node.start_byte() >= start_byte && node.end_byte() <= end_byte {
            return false;
        }
        if let Some(name) = binding_name(dialect, node, source) {
            names.insert(name);
        }
        true
    });
    // The function's own parameters bind for the whole body.
    if let Some(parameters) = function
        .child_by_field_name("parameters")
        .or_else(|| function.child_by_field_name("parameter"))
    {
        walk(parameters, &mut |node| {
            if is_type_position(dialect, node) {
                return false;
            }
            if is_name_node(dialect, node)
                && let Ok(text) = node.utf8_text(source)
            {
                names.insert(text.to_string());
            }
            true
        });
    }
    names
}

/// Names read in the range before the range assigns them.
///
/// The "before" matters: a name the range assigns first is a local of the new
/// function, and passing it in would shadow that assignment with a stale value.
fn names_read_before_assignment(
    dialect: Dialect,
    statements: &[Node],
    source: &[u8],
) -> BTreeSet<String> {
    let mut assigned: BTreeSet<String> = BTreeSet::new();
    let mut read_first: BTreeSet<String> = BTreeSet::new();
    let mut events: Vec<(usize, bool, String)> = Vec::new();
    for statement in statements {
        walk(*statement, &mut |node| {
            if is_type_position(dialect, node) {
                return false;
            }
            if let Some(name) = binding_name(dialect, node, source) {
                let parent = node.parent();
                // An augmented assignment reads its target before writing it.
                if parent.is_some_and(|parent| is_augmented_assignment(dialect, parent.kind())) {
                    events.push((node.start_byte(), false, name.clone()));
                }
                // An assignment's write happens after its right-hand side is
                // evaluated, so `base = base * 2` reads the *outer* `base`.
                // Recording the write at the identifier's own offset would
                // classify that read as reading a local the range had already
                // assigned, and drop a parameter the new function needs.
                let write_at = parent
                    .filter(|parent| is_written_after_evaluation(dialect, parent.kind()))
                    .map(|parent| parent.end_byte())
                    .unwrap_or_else(|| node.start_byte());
                events.push((write_at, true, name));
                return true;
            }
            if is_read_identifier(dialect, node)
                && let Ok(text) = node.utf8_text(source)
            {
                events.push((node.start_byte(), false, text.to_string()));
            }
            true
        });
    }
    events.sort_by_key(|(offset, is_write, _)| (*offset, *is_write));
    for (_, is_write, name) in events {
        if is_write {
            assigned.insert(name);
        } else if !assigned.contains(&name) {
            read_first.insert(name);
        }
    }
    read_first
}

fn names_read_after(
    dialect: Dialect,
    function: Node,
    end_byte: usize,
    source: &[u8],
) -> BTreeSet<String> {
    let mut names = BTreeSet::new();
    walk(function, &mut |node| {
        // Anything that ends at or before the range cannot contain a read that
        // starts after it.
        if node.end_byte() <= end_byte {
            return false;
        }
        if is_type_position(dialect, node) {
            return false;
        }
        if is_read_identifier(dialect, node)
            && node.start_byte() >= end_byte
            && let Ok(text) = node.utf8_text(source)
        {
            names.insert(text.to_string());
        }
        true
    });
    names
}

/// The names bound directly by one scope's own statements.
///
/// Used twice, for two different scopes: the file root, whose names stay free
/// references rather than becoming parameters, and the block the new function
/// is inserted into, whose names it must not collide with.
fn scope_bindings(dialect: Dialect, scope: Node, source: &[u8]) -> BTreeSet<String> {
    let mut names = BTreeSet::new();
    let mut cursor = scope.walk();
    for statement in scope.named_children(&mut cursor) {
        if dialect.is_nested_scope_kind(statement.kind()) {
            if let Some(name) = statement
                .child_by_field_name("name")
                .and_then(|name| name.utf8_text(source).ok())
            {
                names.insert(name.to_string());
            }
            continue;
        }
        walk(statement, &mut |node| {
            if dialect.is_nested_scope_kind(node.kind()) {
                return false;
            }
            if let Some(name) = binding_name(dialect, node, source) {
                names.insert(name);
            }
            true
        });
    }
    names
}

/// Whether this binder's write lands after the rest of the construct is read.
///
/// A loop head is deliberately absent: `for item in items` binds `item` before
/// the body runs, so a body read of `item` is not a read of an outer binding.
fn is_written_after_evaluation(dialect: Dialect, kind: &str) -> bool {
    if is_assignment_kind(dialect, kind) {
        return true;
    }
    match dialect.family {
        Family::Python => false,
        Family::GdScript => matches!(kind, "variable_statement" | "const_statement"),
        Family::JsLike => kind == "variable_declarator",
        Family::Rust => kind == "let_declaration",
    }
}

fn is_augmented_assignment(dialect: Dialect, kind: &str) -> bool {
    match dialect.family {
        Family::Python | Family::GdScript => kind == "augmented_assignment",
        Family::JsLike => kind == "augmented_assignment_expression",
        Family::Rust => kind == "compound_assignment_expr",
    }
}

/// Whether this node names something at all in this dialect, ignoring whether
/// the position reads or binds it.
fn is_name_node(dialect: Dialect, node: Node) -> bool {
    match dialect.family {
        Family::Python | Family::Rust => node.kind() == "identifier",
        Family::GdScript => matches!(node.kind(), "identifier" | "name"),
        Family::JsLike => matches!(
            node.kind(),
            "identifier" | "shorthand_property_identifier" | "shorthand_property_identifier_pattern"
        ),
    }
}

/// A type annotation names types, not values, so nothing inside one is a read
/// or a binding of a runtime name.
fn is_type_position(dialect: Dialect, node: Node) -> bool {
    match dialect.family {
        Family::Python => matches!(node.kind(), "type"),
        Family::GdScript => matches!(node.kind(), "type" | "inferred_type"),
        Family::JsLike => matches!(node.kind(), "type_annotation" | "type_arguments"),
        // Rust spells a type as the `type` field of whatever binds it, with no
        // wrapper node of its own, so the field is the thing to recognize.
        Family::Rust => node
            .parent()
            .and_then(|parent| parent.child_by_field_name("type"))
            .is_some_and(|annotation| annotation.id() == node.id()),
    }
}

/// The name this node binds, if it is a binding position.
fn binding_name(dialect: Dialect, node: Node, source: &[u8]) -> Option<String> {
    if !is_name_node(dialect, node) {
        return None;
    }
    let is_binding = match dialect.family {
        Family::Python => python_binds(dialect, node),
        Family::GdScript => gdscript_binds(node),
        Family::JsLike => js_binds(dialect, node),
        Family::Rust => rust_binds(dialect, node),
    };
    if !is_binding {
        return None;
    }
    node.utf8_text(source).ok().map(str::to_string)
}

fn python_binds(dialect: Dialect, node: Node) -> bool {
    let Some(parent) = node.parent() else {
        return false;
    };
    match parent.kind() {
        // A direct child of the assignment is the target only when it *is* the
        // target: `obj.attr = 1` binds neither `obj` nor `attr`, and treating
        // them as bindings would make a receiver look like a local.
        "assignment" | "augmented_assignment" | "for_statement" => parent
            .child_by_field_name("left")
            .is_some_and(|left| left.id() == node.id()),
        "as_pattern_target" | "aliased_import" => true,
        "function_definition" | "class_definition" => parent
            .child_by_field_name("name")
            .is_some_and(|name| name.id() == node.id()),
        kind if dialect.pattern_kinds().contains(&kind) => pattern_root_binds(dialect, parent),
        _ => false,
    }
}

fn gdscript_binds(node: Node) -> bool {
    let Some(parent) = node.parent() else {
        return false;
    };
    match parent.kind() {
        "variable_statement" | "const_statement" | "function_definition" | "class_definition"
        | "class_name_statement" | "signal_statement" | "enum_definition" => parent
            .child_by_field_name("name")
            .is_some_and(|name| name.id() == node.id()),
        "assignment" | "augmented_assignment" | "for_statement" => parent
            .child_by_field_name("left")
            .is_some_and(|left| left.id() == node.id()),
        "parameters" | "typed_parameter" | "typed_default_parameter" | "default_parameter" => true,
        _ => false,
    }
}

fn js_binds(dialect: Dialect, node: Node) -> bool {
    if node.kind() == "shorthand_property_identifier_pattern" {
        return true;
    }
    let Some(parent) = node.parent() else {
        return false;
    };
    match parent.kind() {
        "variable_declarator" => parent
            .child_by_field_name("name")
            .is_some_and(|name| name.id() == node.id()),
        "assignment_expression" | "augmented_assignment_expression" | "for_in_statement" => parent
            .child_by_field_name("left")
            .is_some_and(|left| left.id() == node.id()),
        "function_declaration" | "generator_function_declaration" | "class_declaration"
        | "function_expression" | "import_specifier" | "namespace_import" | "catch_clause" => parent
            .child_by_field_name("name")
            .or_else(|| parent.child_by_field_name("parameter"))
            .is_some_and(|name| name.id() == node.id()),
        "formal_parameters" | "required_parameter" | "optional_parameter" => true,
        "arrow_function" => parent
            .child_by_field_name("parameter")
            .is_some_and(|name| name.id() == node.id()),
        kind if dialect.pattern_kinds().contains(&kind) => js_pattern_root_binds(dialect, parent),
        _ => false,
    }
}

/// Whether a chain of pattern nodes bottoms out in a binding target.
fn pattern_root_binds(dialect: Dialect, node: Node) -> bool {
    let Some(parent) = node.parent() else {
        return false;
    };
    if dialect.pattern_kinds().contains(&parent.kind()) {
        return pattern_root_binds(dialect, parent);
    }
    match parent.kind() {
        "assignment" | "for_statement" => parent
            .child_by_field_name("left")
            .is_some_and(|left| left.id() == node.id()),
        _ => false,
    }
}

fn js_pattern_root_binds(dialect: Dialect, node: Node) -> bool {
    let Some(parent) = node.parent() else {
        return false;
    };
    if dialect.pattern_kinds().contains(&parent.kind()) {
        return js_pattern_root_binds(dialect, parent);
    }
    match parent.kind() {
        "variable_declarator" => parent
            .child_by_field_name("name")
            .is_some_and(|name| name.id() == node.id()),
        "assignment_expression" | "for_in_statement" => parent
            .child_by_field_name("left")
            .is_some_and(|left| left.id() == node.id()),
        "formal_parameters" | "required_parameter" | "optional_parameter" | "arrow_function" => {
            true
        }
        _ => false,
    }
}

/// Rust binding positions.
///
/// Every one of them is a `pattern` or a `name` field, which is what makes the
/// receiver case cheap to get right: `self` is its own node kind, never an
/// `identifier`, so it can never be mistaken for a name a signature could
/// carry.
fn rust_binds(dialect: Dialect, node: Node) -> bool {
    let Some(parent) = node.parent() else {
        return false;
    };
    match parent.kind() {
        "let_declaration" | "for_expression" | "parameter" | "closure_parameters" => parent
            .child_by_field_name("pattern")
            .is_some_and(|pattern| pattern.id() == node.id())
            || parent.kind() == "closure_parameters",
        "assignment_expression" | "compound_assignment_expr" => parent
            .child_by_field_name("left")
            .is_some_and(|left| left.id() == node.id()),
        "function_item" | "const_item" | "static_item" | "mod_item" => parent
            .child_by_field_name("name")
            .is_some_and(|name| name.id() == node.id()),
        kind if dialect.pattern_kinds().contains(&kind) => rust_pattern_root_binds(dialect, parent),
        _ => false,
    }
}

fn rust_pattern_root_binds(dialect: Dialect, node: Node) -> bool {
    let Some(parent) = node.parent() else {
        return false;
    };
    if dialect.pattern_kinds().contains(&parent.kind()) {
        return rust_pattern_root_binds(dialect, parent);
    }
    match parent.kind() {
        "let_declaration" | "for_expression" | "parameter" => parent
            .child_by_field_name("pattern")
            .is_some_and(|pattern| pattern.id() == node.id()),
        "closure_parameters" => true,
        _ => false,
    }
}

fn rust_reads(dialect: Dialect, node: Node, parent: Node) -> bool {
    match parent.kind() {
        // `value.field` — the field is a `field_identifier`, so only the
        // receiver reaches here.
        "field_expression" => parent
            .child_by_field_name("field")
            .is_none_or(|field| field.id() != node.id()),
        "let_declaration" | "for_expression" | "parameter" => parent
            .child_by_field_name("pattern")
            .is_none_or(|pattern| !covers(pattern, node)),
        "assignment_expression" | "compound_assignment_expr" => parent
            .child_by_field_name("left")
            .is_none_or(|left| !covers(left, node)),
        "function_item" | "const_item" | "static_item" | "mod_item" => parent
            .child_by_field_name("name")
            .is_none_or(|name| name.id() != node.id()),
        "closure_parameters" => false,
        kind if dialect.pattern_kinds().contains(&kind) => !rust_pattern_root_binds(dialect, parent),
        _ => true,
    }
}

/// Whether this identifier reads a binding, as opposed to naming a member, a
/// keyword argument, or a binding position.
fn is_read_identifier(dialect: Dialect, node: Node) -> bool {
    if !is_name_node(dialect, node) {
        return false;
    }
    if node.kind() == "shorthand_property_identifier_pattern" {
        return false;
    }
    let Some(parent) = node.parent() else {
        return false;
    };
    match dialect.family {
        Family::Python => python_reads(dialect, node, parent),
        Family::GdScript => gdscript_reads(node, parent),
        Family::JsLike => js_reads(dialect, node, parent),
        Family::Rust => rust_reads(dialect, node, parent),
    }
}

fn python_reads(dialect: Dialect, node: Node, parent: Node) -> bool {
    match parent.kind() {
        // `obj.name` — the member is not a binding in this scope.
        "attribute" => parent
            .child_by_field_name("attribute")
            .is_none_or(|attribute| attribute.id() != node.id()),
        // `f(name=value)` — the keyword is the callee's parameter name.
        "keyword_argument" => parent
            .child_by_field_name("name")
            .is_none_or(|name| name.id() != node.id()),
        "assignment" | "for_statement" => parent
            .child_by_field_name("left")
            .is_none_or(|left| !covers(left, node)),
        // An augmented assignment reads its target; `names_read_before_assignment`
        // records that read explicitly, so it is not double-counted here.
        "augmented_assignment" => parent
            .child_by_field_name("left")
            .is_none_or(|left| !covers(left, node)),
        "function_definition" | "class_definition" => parent
            .child_by_field_name("name")
            .is_none_or(|name| name.id() != node.id()),
        "parameters" | "default_parameter" | "typed_parameter" | "as_pattern_target"
        | "aliased_import" => false,
        kind if dialect.pattern_kinds().contains(&kind) => !pattern_root_binds(dialect, parent),
        _ => true,
    }
}

fn gdscript_reads(node: Node, parent: Node) -> bool {
    // GDScript spells binding positions with a `name` node and reads with an
    // `identifier`, so most of the work is already done by the grammar.
    if node.kind() == "name" {
        return false;
    }
    match parent.kind() {
        // `(attribute (identifier) (identifier))` carries no field names: the
        // first child is the receiver and reads, the rest are members.
        "attribute" => parent
            .named_child(0)
            .is_some_and(|object| object.id() == node.id()),
        "assignment" | "augmented_assignment" | "for_statement" => parent
            .child_by_field_name("left")
            .is_none_or(|left| left.id() != node.id()),
        "parameters" | "typed_parameter" | "typed_default_parameter" | "default_parameter"
        | "type" | "inferred_type" => false,
        _ => true,
    }
}

fn js_reads(dialect: Dialect, node: Node, parent: Node) -> bool {
    match parent.kind() {
        // `obj.name` — the property is spelled `property_identifier`, so only a
        // computed member's index reaches here, and that does read.
        "member_expression" => parent
            .child_by_field_name("property")
            .is_none_or(|property| property.id() != node.id()),
        "variable_declarator" => parent
            .child_by_field_name("name")
            .is_none_or(|name| name.id() != node.id()),
        "assignment_expression" | "augmented_assignment_expression" | "for_in_statement" => parent
            .child_by_field_name("left")
            .is_none_or(|left| !covers(left, node)),
        "function_declaration" | "generator_function_declaration" | "class_declaration"
        | "function_expression" => parent
            .child_by_field_name("name")
            .is_none_or(|name| name.id() != node.id()),
        "formal_parameters" | "required_parameter" | "optional_parameter" | "import_specifier"
        | "namespace_import" | "catch_clause" | "labeled_statement" => false,
        "arrow_function" => parent
            .child_by_field_name("parameter")
            .is_none_or(|name| name.id() != node.id()),
        kind if dialect.pattern_kinds().contains(&kind) => !js_pattern_root_binds(dialect, parent),
        _ => true,
    }
}

fn covers(outer: Node, inner: Node) -> bool {
    outer.id() == inner.id()
        || (outer.start_byte() <= inner.start_byte() && outer.end_byte() >= inner.end_byte())
}

/// Pre-order walk; the visitor returns `false` to skip a subtree.
fn walk(node: Node, visit: &mut impl FnMut(Node) -> bool) {
    if !visit(node) {
        return;
    }
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        walk(child, visit);
    }
}

/// The whitespace prefix of the line `byte` sits on.
fn line_indent(source: &[u8], byte: usize) -> String {
    let line_start = source[..byte]
        .iter()
        .rposition(|byte| *byte == b'\n')
        .map(|position| position + 1)
        .unwrap_or(0);
    String::from_utf8_lossy(&source[line_start..byte])
        .chars()
        .take_while(|character| character.is_whitespace())
        .collect()
}

#[cfg(all(test, feature = "lang-python"))]
mod python_tests {
    use super::*;

    const SOURCE: &str = "TOTAL = 10\n\n\ndef outer(base, scale):\n    prefix = base * 2\n    acc = 0\n    for item in range(scale):\n        acc += item * prefix\n    label = f\"{acc}\"\n    return label, acc, TOTAL\n";

    fn plan(start: usize, end: usize, name: &str) -> Result<ExtractionPlan, ExtractionRefusal> {
        plan_extraction(Lang::Python, SOURCE.as_bytes(), start, end, name)
    }

    #[test]
    fn derives_parameters_from_outer_bindings_and_returns_from_later_reads() {
        // Lines 5-7: `acc = 0` and the `for` loop. `prefix` and `scale` come
        // from outside; `acc` is assigned here and read on line 8.
        let plan = plan(5, 7, "accumulate").expect("planned");

        assert_eq!(plan.enclosing_function, "outer");
        assert_eq!(
            plan.parameters,
            vec!["prefix".to_string(), "scale".to_string()]
        );
        assert_eq!(plan.parameter_spellings, plan.parameters);
        assert_eq!(plan.returns, vec!["acc".to_string()]);
        assert_eq!(plan.indent, "    ");
        assert_eq!(plan.indent_unit, "    ");
        // Python has no declarations, so a call site never declares.
        assert!(!plan.returns_need_declaration);
    }

    #[test]
    fn a_name_the_range_assigns_first_is_a_local_not_a_parameter() {
        // `acc` is bound *outside* the range on line 1, so the outer-binding
        // test alone would make it a parameter. It is not one: line 2 assigns
        // it before line 3 reads it, so passing the outer value in would feed
        // the extracted body a value it immediately overwrites, and the caller
        // would keep computing an argument nothing reads.
        let source =
            "def outer(base):\n    acc = 0\n    acc = base * 2\n    acc += 1\n    return acc\n";
        let plan =
            plan_extraction(Lang::Python, source.as_bytes(), 2, 3, "recompute").expect("planned");

        assert_eq!(plan.parameters, vec!["base".to_string()]);
        assert_eq!(plan.returns, vec!["acc".to_string()]);
    }

    #[test]
    fn a_module_scope_name_is_neither_parameter_nor_return() {
        // Extract only the `label` line, which reads `acc` and `TOTAL`. `TOTAL`
        // is bound at module scope, so it is not in the enclosing function's
        // bindings and stays a free reference. Threading it through a parameter
        // would compile and quietly change what the new function closes over —
        // which is also why `plan_extraction` checks module scope for the
        // *collision* case rather than trusting "not bound in the function".
        let plan = plan(8, 8, "describe").expect("planned");
        assert_eq!(plan.parameters, vec!["acc".to_string()]);
        assert!(!plan.parameters.contains(&"TOTAL".to_string()));
        assert_eq!(plan.returns, vec!["label".to_string()]);
    }

    #[test]
    fn an_attribute_assignment_binds_neither_the_receiver_nor_the_member() {
        // `cfg.limit = base` writes through `cfg`; it does not rebind it. If
        // the receiver counted as assigned-in-range it would stop being a
        // parameter, and the extracted function would write to a name it never
        // received — which raises at the call site rather than at review.
        let source = "def outer(cfg, base):\n    cfg.limit = base\n    return cfg\n";
        let plan =
            plan_extraction(Lang::Python, source.as_bytes(), 1, 1, "configure").expect("planned");
        assert_eq!(
            plan.parameters,
            vec!["base".to_string(), "cfg".to_string()]
        );
        assert!(plan.returns.is_empty(), "{:?}", plan.returns);
    }

    #[test]
    fn refuses_a_range_whose_control_flow_escapes() {
        assert_eq!(
            plan(9, 9, "finish"),
            Err(ExtractionRefusal::ControlFlowEscapes("return"))
        );
    }

    #[test]
    fn a_break_bound_to_a_loop_inside_the_range_does_not_escape() {
        // The loop moves with the range, so its `break` still breaks the same
        // loop. Refusing here declined the most ordinary extraction there is.
        let source = "def outer(items, limit):\n    total = 0\n    for item in items:\n        if item > limit:\n            break\n        total += item\n    return total\n";
        let plan =
            plan_extraction(Lang::Python, source.as_bytes(), 1, 5, "sum_until").expect("planned");
        assert_eq!(plan.parameters, vec!["items".to_string(), "limit".to_string()]);
        assert_eq!(plan.returns, vec!["total".to_string()]);
    }

    #[test]
    fn a_break_bound_to_a_loop_outside_the_range_still_escapes() {
        // Same keyword, opposite answer: the loop stays behind, so hoisting the
        // `break` changes which construct it leaves.
        let source = "def outer(items, limit):\n    total = 0\n    for item in items:\n        if item > limit:\n            break\n        total += item\n    return total\n";
        assert_eq!(
            plan_extraction(Lang::Python, source.as_bytes(), 3, 5, "accumulate"),
            Err(ExtractionRefusal::ControlFlowEscapes("break"))
        );
    }

    #[test]
    fn a_continue_bound_to_a_loop_inside_the_range_does_not_escape() {
        let source = "def outer(items):\n    total = 0\n    for item in items:\n        if item < 0:\n            continue\n        total += item\n    return total\n";
        let plan =
            plan_extraction(Lang::Python, source.as_bytes(), 1, 5, "sum_positive").expect("planned");
        assert_eq!(plan.returns, vec!["total".to_string()]);
    }

    #[test]
    fn refuses_a_range_outside_any_function() {
        assert_eq!(
            plan(0, 0, "setup"),
            Err(ExtractionRefusal::NotInsideFunction)
        );
    }

    #[test]
    fn refuses_an_empty_range() {
        assert_eq!(plan(1, 2, "nothing"), Err(ExtractionRefusal::EmptyRange));
    }

    #[test]
    fn refuses_a_name_that_already_binds_at_module_scope() {
        assert_eq!(
            plan(5, 7, "TOTAL"),
            Err(ExtractionRefusal::NameCollision("TOTAL".to_string()))
        );
    }

    #[test]
    fn refuses_a_name_that_already_binds_in_the_enclosing_function() {
        assert_eq!(
            plan(5, 7, "prefix"),
            Err(ExtractionRefusal::NameCollision("prefix".to_string()))
        );
    }

    #[test]
    fn refuses_when_the_range_assigns_a_global_declared_name() {
        let source =
            "COUNT = 0\n\n\ndef outer():\n    global COUNT\n    COUNT = 1\n    return COUNT\n";
        assert_eq!(
            plan_extraction(Lang::Python, source.as_bytes(), 5, 5, "bump"),
            Err(ExtractionRefusal::RebindsOuterScope("COUNT".to_string()))
        );
    }

    #[test]
    fn a_method_extraction_lands_past_the_class_with_self_as_a_parameter() {
        // A `def` placed *beside* a method is another method, and the bare call
        // left in its place does not resolve to it. Climbing past the class
        // puts it where the call can see it, and `self` — a name like any
        // other in Python — threads through the signature.
        let source = "class Panel:\n    def outer(self, base):\n        acc = self.scale * base\n        return acc\n";
        let plan =
            plan_extraction(Lang::Python, source.as_bytes(), 2, 2, "double").expect("planned");
        assert_eq!(plan.enclosing_function, "outer");
        // Receiver first: alphabetical order would read as a mistake.
        assert_eq!(
            plan.parameters,
            vec!["self".to_string(), "base".to_string()]
        );
        // Module scope, not class scope.
        assert_eq!(plan.enclosing_indent, "");
        assert_eq!(plan.insert_byte, source.len() - 1);
        let (function, call) = render_extraction(&plan, source, "double");
        assert!(function.contains("\ndef double(self, base):"), "{function}");
        assert!(
            function.contains("\n    acc = self.scale * base"),
            "{function}"
        );
        assert_eq!(call, "        acc = double(self, base)");
    }

    #[test]
    fn a_nested_function_extraction_stays_inside_its_enclosing_function() {
        // Not every climb is out to module scope: a nested `def` closes over
        // the outer function's locals, and hoisting past it would leave the
        // extracted body reading names that are no longer in scope.
        let source = "def outer(a):\n    scale = 2\n\n    def inner(b):\n        acc = scale * b\n        return acc\n    return inner\n";
        let plan =
            plan_extraction(Lang::Python, source.as_bytes(), 4, 4, "double").expect("planned");
        assert_eq!(plan.enclosing_function, "inner");
        assert_eq!(plan.enclosing_indent, "    ");
        // `scale` belongs to `outer`, not to `inner`, so it stays a free
        // reference the sibling `def` can still see.
        assert_eq!(plan.parameters, vec!["b".to_string()]);
    }

    #[test]
    fn renders_a_def_and_a_destructuring_call_that_agree() {
        let plan = plan(5, 7, "accumulate").expect("planned");
        let (function, call) = render_extraction(&plan, SOURCE, "accumulate");

        assert!(
            function.contains("def accumulate(prefix, scale):"),
            "{function}"
        );
        assert!(function.contains("    acc = 0"), "{function}");
        assert!(
            function.contains("        acc += item * prefix"),
            "{function}"
        );
        assert!(function.contains("    return acc"), "{function}");
        assert_eq!(call, "    acc = accumulate(prefix, scale)");
    }

    #[test]
    fn renders_a_bare_call_when_nothing_is_read_afterwards() {
        let source = "def outer(scale):\n    total = 0\n    print(scale)\n    return total\n";
        let plan =
            plan_extraction(Lang::Python, source.as_bytes(), 2, 2, "report").expect("planned");
        assert!(plan.returns.is_empty(), "{:?}", plan.returns);
        let (function, call) = render_extraction(&plan, source, "report");
        assert!(function.contains("def report(scale):"), "{function}");
        assert!(!function.contains("return"), "{function}");
        assert_eq!(call, "    report(scale)");
    }

    #[test]
    fn indentation_is_measured_from_the_file_rather_than_assumed() {
        // Two-space Python is unusual and legal. Emitting four spaces here
        // would still parse — and would leave a file that no longer agrees
        // with itself.
        let source = "def outer(base):\n  acc = base * 2\n  return acc\n";
        let plan =
            plan_extraction(Lang::Python, source.as_bytes(), 1, 1, "double").expect("planned");
        assert_eq!(plan.indent_unit, "  ");
        let (function, _) = render_extraction(&plan, source, "double");
        assert!(function.contains("\n  acc = base * 2"), "{function}");
    }
}

#[cfg(all(test, feature = "lang-gdscript"))]
mod gdscript_tests {
    use super::*;

    const SOURCE: &str = "const TOTAL = 10\n\nfunc outer(base, scale):\n\tvar prefix = base * 2\n\tvar acc = 0\n\tfor item in range(scale):\n\t\tacc += item * prefix\n\treturn acc\n";

    #[test]
    fn derives_the_same_signature_the_python_core_does() {
        let plan =
            plan_extraction(Lang::GdScript, SOURCE.as_bytes(), 4, 6, "accumulate").expect("planned");
        assert_eq!(plan.enclosing_function, "outer");
        assert_eq!(
            plan.parameters,
            vec!["prefix".to_string(), "scale".to_string()]
        );
        assert_eq!(plan.returns, vec!["acc".to_string()]);
        assert_eq!(plan.indent_unit, "\t");
        // `var acc` left with the range, so the call site has to declare it.
        assert!(plan.returns_need_declaration);
    }

    #[test]
    fn renders_a_func_and_a_var_call_that_agree() {
        let plan =
            plan_extraction(Lang::GdScript, SOURCE.as_bytes(), 4, 6, "accumulate").expect("planned");
        let (function, call) = render_extraction(&plan, SOURCE, "accumulate");
        assert!(
            function.contains("func accumulate(prefix, scale):"),
            "{function}"
        );
        assert!(function.contains("\n\tvar acc = 0"), "{function}");
        assert!(
            function.contains("\n\t\tacc += item * prefix"),
            "{function}"
        );
        assert!(function.contains("\n\treturn acc"), "{function}");
        assert_eq!(call, "\tvar acc = accumulate(prefix, scale)");
    }

    #[test]
    fn assigns_rather_than_declares_when_the_name_outlives_the_range() {
        // `acc` is declared before the range, so the range only assigns it. A
        // call site that said `var acc = ...` would shadow the outer binding
        // and the later read would see the stale value.
        let source = "func outer(base):\n\tvar acc = 0\n\tacc = base * 2\n\treturn acc\n";
        let plan =
            plan_extraction(Lang::GdScript, source.as_bytes(), 2, 2, "double").expect("planned");
        assert!(!plan.returns_need_declaration);
        // The declaration stayed behind, so the *new* function has to make one:
        // its body assigns `acc`, and GDScript will not accept that bare.
        assert_eq!(plan.local_declarations, vec!["acc".to_string()]);
        let (function, call) = render_extraction(&plan, source, "double");
        assert!(function.contains("\n\tvar acc\n\tacc = base * 2"), "{function}");
        assert_eq!(call, "\tacc = double(base)");
    }

    #[test]
    fn refuses_a_range_that_assigns_a_file_scope_var() {
        // A bare `acc = ...` inside a `func` writes the script's own `var acc`.
        // Declaring it in the new function would shadow that member and the
        // write would stop being visible — a change no reader would see.
        let source = "var acc = 0\n\nfunc outer(base):\n\tacc = base * 2\n\treturn acc\n";
        assert_eq!(
            plan_extraction(Lang::GdScript, source.as_bytes(), 3, 3, "double"),
            Err(ExtractionRefusal::AssignsUndeclaredName("acc".to_string()))
        );
    }

    #[test]
    fn a_write_through_a_member_leaves_the_receiver_a_parameter() {
        let source = "func outer(cfg, base):\n\tcfg.limit = base\n\treturn cfg\n";
        let plan =
            plan_extraction(Lang::GdScript, source.as_bytes(), 1, 1, "configure").expect("planned");
        assert_eq!(
            plan.parameters,
            vec!["base".to_string(), "cfg".to_string()]
        );
        assert!(plan.returns.is_empty(), "{:?}", plan.returns);
    }

    #[test]
    fn refuses_more_than_one_returned_name() {
        // GDScript has no destructuring assignment, so two values cannot reach
        // one call site. Emitting an array and two index reads would be three
        // statements where the caller wrote one.
        let source =
            "func outer(base):\n\tvar a = base\n\tvar b = base + 1\n\treturn a + b\n";
        assert_eq!(
            plan_extraction(Lang::GdScript, source.as_bytes(), 1, 2, "split"),
            Err(ExtractionRefusal::MultipleReturnsUnsupported("gdscript"))
        );
    }

    #[test]
    fn a_method_extraction_stays_inside_the_class_as_a_sibling_func() {
        // The opposite of Python and JavaScript, and for the opposite reason:
        // GDScript resolves a bare call against the script's own members, so a
        // sibling `func` is exactly what the call left behind needs. Climbing
        // out of the class would break the call rather than fix it.
        let source = "class Panel:\n\tfunc outer(base):\n\t\tvar acc = base * 2\n\t\treturn acc\n";
        let plan =
            plan_extraction(Lang::GdScript, source.as_bytes(), 2, 2, "double").expect("planned");
        assert_eq!(plan.enclosing_function, "outer");
        assert_eq!(plan.enclosing_indent, "\t");
        let (function, call) = render_extraction(&plan, source, "double");
        assert!(function.contains("\n\tfunc double(base):"), "{function}");
        assert!(function.contains("\n\t\tvar acc = base * 2"), "{function}");
        assert_eq!(call, "\t\tvar acc = double(base)");
    }

    #[test]
    fn refuses_a_name_that_already_binds_as_a_sibling_method() {
        // The new `func` lands inside the class, so the names it must not
        // collide with are the class's own members — which a file-root scan
        // never sees.
        let source = "class Panel:\n\tfunc double(x):\n\t\treturn x\n\n\tfunc outer(base):\n\t\tvar acc = base * 2\n\t\treturn acc\n";
        assert_eq!(
            plan_extraction(Lang::GdScript, source.as_bytes(), 5, 5, "double"),
            Err(ExtractionRefusal::NameCollision("double".to_string()))
        );
    }

    #[test]
    fn refuses_a_range_whose_control_flow_escapes() {
        assert_eq!(
            plan_extraction(Lang::GdScript, SOURCE.as_bytes(), 7, 7, "finish"),
            Err(ExtractionRefusal::ControlFlowEscapes("return"))
        );
    }
}

#[cfg(all(test, feature = "lang-javascript"))]
mod javascript_tests {
    use super::*;

    const SOURCE: &str = "const TOTAL = 10;\n\nfunction outer(base, scale) {\n  const prefix = base * 2;\n  let acc = 0;\n  for (const item of range(scale)) {\n    acc += item * prefix;\n  }\n  return acc + TOTAL;\n}\n";

    #[test]
    fn derives_the_same_signature_the_python_core_does() {
        let plan = plan_extraction(Lang::JavaScript, SOURCE.as_bytes(), 4, 7, "accumulate")
            .expect("planned");
        assert_eq!(plan.enclosing_function, "outer");
        assert_eq!(
            plan.parameters,
            vec!["prefix".to_string(), "scale".to_string()]
        );
        assert_eq!(plan.returns, vec!["acc".to_string()]);
        assert_eq!(plan.indent_unit, "  ");
        assert!(plan.returns_need_declaration);
    }

    #[test]
    fn renders_a_function_and_a_let_call_that_agree() {
        let plan = plan_extraction(Lang::JavaScript, SOURCE.as_bytes(), 4, 7, "accumulate")
            .expect("planned");
        let (function, call) = render_extraction(&plan, SOURCE, "accumulate");
        assert!(
            function.contains("function accumulate(prefix, scale) {"),
            "{function}"
        );
        assert!(function.contains("\n  let acc = 0;"), "{function}");
        assert!(function.contains("\n    acc += item * prefix;"), "{function}");
        assert!(function.contains("\n  return acc;"), "{function}");
        assert!(function.ends_with("}\n"), "{function}");
        assert_eq!(call, "  let acc = accumulate(prefix, scale);");
    }

    #[test]
    fn several_returned_names_become_one_array_destructuring() {
        let source = "function outer(base) {\n  let a = base;\n  let b = base + 1;\n  return a + b;\n}\n";
        let plan =
            plan_extraction(Lang::JavaScript, source.as_bytes(), 1, 2, "split").expect("planned");
        assert_eq!(plan.returns, vec!["a".to_string(), "b".to_string()]);
        let (function, call) = render_extraction(&plan, source, "split");
        assert!(function.contains("return [a, b];"), "{function}");
        assert_eq!(call, "  let [a, b] = split(base);");
    }

    #[test]
    fn a_bare_call_still_ends_in_a_semicolon() {
        let source = "function outer(scale) {\n  report(scale);\n  return 1;\n}\n";
        let plan =
            plan_extraction(Lang::JavaScript, source.as_bytes(), 1, 1, "announce").expect("planned");
        assert!(plan.returns.is_empty(), "{:?}", plan.returns);
        let (_, call) = render_extraction(&plan, source, "announce");
        assert_eq!(call, "  announce(scale);");
    }

    #[test]
    fn a_property_write_leaves_the_receiver_a_parameter() {
        let source = "function outer(cfg, base) {\n  cfg.limit = base;\n  return cfg;\n}\n";
        let plan =
            plan_extraction(Lang::JavaScript, source.as_bytes(), 1, 1, "configure").expect("planned");
        assert_eq!(
            plan.parameters,
            vec!["base".to_string(), "cfg".to_string()]
        );
        assert!(plan.returns.is_empty(), "{:?}", plan.returns);
    }

    #[test]
    fn refuses_a_range_that_mixes_new_and_existing_names() {
        // `a` exists before the range and `b` is created inside it. One call
        // site cannot both assign and declare, and two would rebind `a` before
        // the second statement read it.
        let source = "function outer(base) {\n  let a = 0;\n  a = base;\n  let b = base + 1;\n  return a + b;\n}\n";
        assert_eq!(
            plan_extraction(Lang::JavaScript, source.as_bytes(), 2, 3, "split"),
            Err(ExtractionRefusal::MixedReturnDeclarations)
        );
    }

    #[test]
    fn refuses_a_range_that_assigns_a_module_scope_binding() {
        let source = "let total = 0;\n\nfunction outer(base) {\n  total = base * 2;\n  return total;\n}\n";
        assert_eq!(
            plan_extraction(Lang::JavaScript, source.as_bytes(), 3, 3, "double"),
            Err(ExtractionRefusal::AssignsUndeclaredName("total".to_string()))
        );
    }

    #[test]
    fn a_self_referential_assignment_keeps_its_target_a_parameter() {
        // `base = base * 2` reads the *outer* `base` before writing it. An
        // ordering that recorded the write at the target's own offset would
        // classify that read as reading a local the range had already assigned,
        // drop `base` from the signature, and emit a function that multiplies
        // `undefined`.
        let source =
            "function outer(base) {\n  base = base * 2;\n  return base;\n}\n";
        let plan =
            plan_extraction(Lang::JavaScript, source.as_bytes(), 1, 1, "double").expect("planned");
        assert_eq!(plan.parameters, vec!["base".to_string()]);
        assert_eq!(plan.returns, vec!["base".to_string()]);
        // Already declared by the signature, so no prologue.
        assert!(plan.local_declarations.is_empty());
        let (function, call) = render_extraction(&plan, source, "double");
        assert!(function.contains("function double(base) {"), "{function}");
        assert_eq!(call, "  base = double(base);");
    }

    #[test]
    fn a_method_extraction_lands_beside_the_class_declaration() {
        let source =
            "class Panel {\n  outer(base) {\n    let acc = base * 2;\n    return acc;\n  }\n}\n";
        let plan =
            plan_extraction(Lang::JavaScript, source.as_bytes(), 2, 2, "double").expect("planned");
        assert_eq!(plan.enclosing_function, "outer");
        // Beside the class, at the class's own indentation — not inside the
        // class body, where `function double(...)` is not even legal.
        assert_eq!(plan.enclosing_indent, "");
        assert_eq!(plan.insert_byte, source.len() - 1);
        let (function, call) = render_extraction(&plan, source, "double");
        assert!(function.contains("\nfunction double(base) {"), "{function}");
        assert_eq!(call, "    let acc = double(base);");
    }

    #[test]
    fn a_break_bound_to_a_switch_inside_the_range_does_not_escape() {
        // JavaScript's `break` binds to a `switch` as well as to a loop, so the
        // loop-depth test alone would refuse a hoisted switch that is entirely
        // self-contained.
        let source = "function outer(kind) {\n  let label = \"\";\n  switch (kind) {\n    case 1:\n      label = \"one\";\n      break;\n    default:\n      label = \"other\";\n  }\n  return label;\n}\n";
        let plan =
            plan_extraction(Lang::JavaScript, source.as_bytes(), 2, 8, "describe").expect("planned");
        assert_eq!(plan.parameters, vec!["kind".to_string()]);
        assert_eq!(plan.returns, vec!["label".to_string()]);
    }

    #[test]
    fn a_labelled_break_targeting_a_label_outside_the_range_escapes() {
        // The inner loop moves with the range, so an unlabelled `break` would
        // be fine — but this one jumps to a label that stays behind, and no
        // signature carries a jump out of two loops.
        let source = "function outer(rows) {\n  let hits = 0;\n  outer: for (const row of rows) {\n    for (const cell of row) {\n      if (cell) {\n        break outer;\n      }\n      hits += 1;\n    }\n  }\n  return hits;\n}\n";
        assert_eq!(
            plan_extraction(Lang::JavaScript, source.as_bytes(), 3, 8, "scan"),
            Err(ExtractionRefusal::ControlFlowEscapes("break"))
        );
    }

    #[test]
    fn a_labelled_break_whose_label_is_inside_the_range_does_not_escape() {
        let source = "function outer(rows) {\n  let hits = 0;\n  outer: for (const row of rows) {\n    for (const cell of row) {\n      if (cell) {\n        break outer;\n      }\n      hits += 1;\n    }\n  }\n  return hits;\n}\n";
        let plan =
            plan_extraction(Lang::JavaScript, source.as_bytes(), 1, 9, "scan").expect("planned");
        assert_eq!(plan.parameters, vec!["rows".to_string()]);
        assert_eq!(plan.returns, vec!["hits".to_string()]);
        assert!(plan.returns_need_declaration);
    }

    #[test]
    fn refuses_a_method_extraction_that_uses_this() {
        // `this` is not a name a derived signature can carry, and a plain
        // function's `this` is not the method's. Threading it would take a
        // body rewrite the derivation does not do, so it refuses instead of
        // emitting a function that reads a different receiver.
        let source = "class Panel {\n  outer(base) {\n    let acc = this.scale * base;\n    return acc;\n  }\n}\n";
        assert_eq!(
            plan_extraction(Lang::JavaScript, source.as_bytes(), 2, 2, "double"),
            Err(ExtractionRefusal::ReferencesReceiver("this"))
        );
    }

    #[test]
    fn refuses_to_hoist_out_of_an_arrow_function() {
        // `const view = () => {...}` has no statement position beside the
        // arrow: inserting there would land inside the declaration.
        let source = "const view = (base) => {\n  let acc = base * 2;\n  return acc;\n};\n";
        assert_eq!(
            plan_extraction(Lang::JavaScript, source.as_bytes(), 1, 1, "double"),
            Err(ExtractionRefusal::EnclosingFunctionNotHoistable)
        );
    }
}

#[cfg(all(test, feature = "lang-typescript"))]
mod typescript_tests {
    use super::*;

    #[test]
    fn copies_an_existing_annotation_into_the_new_signature() {
        let source = "function outer(base: number, scale: number) {\n  let acc = 0;\n  acc = base * scale;\n  return acc;\n}\n";
        let plan = plan_extraction(Lang::TypeScript, source.as_bytes(), 2, 2, "combine")
            .expect("planned");
        assert_eq!(
            plan.parameters,
            vec!["base".to_string(), "scale".to_string()]
        );
        assert_eq!(
            plan.parameter_spellings,
            vec!["base: number".to_string(), "scale: number".to_string()]
        );
        // `acc` was declared outside the range, so its declaration did not move
        // with the statements. Without a prologue the emitted body assigns a
        // name that is not in scope: it type-checks as `Cannot find name 'acc'`
        // and, in plain JS, silently creates a global.
        assert_eq!(plan.local_declarations, vec!["acc".to_string()]);
        let (function, call) = render_extraction(&plan, source, "combine");
        assert!(
            function.contains("function combine(base: number, scale: number) {"),
            "{function}"
        );
        assert!(
            function.contains("\n  let acc;\n  acc = base * scale;"),
            "{function}"
        );
        assert_eq!(call, "  acc = combine(base, scale);");
    }

    #[test]
    fn refuses_a_parameter_whose_type_cannot_be_copied() {
        // The alternative is `unknown`, or nothing at all under
        // `noImplicitAny`. Both type-check something other than what the code
        // does, which is exactly the failure an extraction must not ship.
        let source =
            "function outer(base) {\n  let acc = 0;\n  acc = base * 2;\n  return acc;\n}\n";
        assert_eq!(
            plan_extraction(Lang::TypeScript, source.as_bytes(), 2, 2, "double"),
            Err(ExtractionRefusal::UnspellableParameterType(
                "base".to_string()
            ))
        );
    }

    #[test]
    fn copies_a_generic_annotation_verbatim() {
        let source = "function outer(rows: Map<string, number>) {\n  let total = 0;\n  total = rows.size;\n  return total;\n}\n";
        let plan =
            plan_extraction(Lang::TypeScript, source.as_bytes(), 2, 2, "count").expect("planned");
        assert_eq!(
            plan.parameter_spellings,
            vec!["rows: Map<string, number>".to_string()]
        );
    }
}

#[cfg(all(test, feature = "lang-rust"))]
mod rust_tests {
    use super::*;

    // `rows` is moved in and never read again; `total` is threaded in and
    // handed back, which is what keeps the extraction by-value.
    const SOURCE: &str = "fn outer(rows: &[u32], limit: u32) -> u32 {\n    let mut total: u32 = 0;\n    for row in rows {\n        total += row * limit;\n    }\n    total\n}\n";

    #[test]
    fn copies_annotations_and_threads_an_accumulator_by_value() {
        let plan = plan_extraction(Lang::Rust, SOURCE.as_bytes(), 2, 4, "accumulate")
            .expect("planned");
        assert_eq!(plan.enclosing_function, "outer");
        assert_eq!(
            plan.parameters,
            vec!["limit".to_string(), "rows".to_string(), "total".to_string()]
        );
        // `total` is assigned in the range, so the signature says `mut`; the
        // other two are read-only and do not.
        assert_eq!(
            plan.parameter_spellings,
            vec![
                "limit: u32".to_string(),
                "rows: &[u32]".to_string(),
                "mut total: u32".to_string()
            ]
        );
        assert_eq!(plan.returns, vec!["total".to_string()]);
        assert_eq!(plan.return_type, Some("u32".to_string()));

        let (function, call) = render_extraction(&plan, SOURCE, "accumulate");
        assert!(
            function.contains("fn accumulate(limit: u32, rows: &[u32], mut total: u32) -> u32 {"),
            "{function}"
        );
        assert!(function.contains("\n    for row in rows {"), "{function}");
        // A trailing expression, not `return`.
        assert!(function.contains("\n    total\n"), "{function}");
        assert!(!function.contains("return"), "{function}");
        assert_eq!(call, "    total = accumulate(limit, rows, total);");
    }

    #[test]
    fn refuses_a_name_it_would_move_and_the_caller_still_reads() {
        // `rows` is read after the range, so moving it in would leave the
        // caller reading a moved value. Borrowing instead would mean rewriting
        // every use in the body into a dereference.
        let source = "fn outer(rows: &[u32]) -> usize {\n    let mut total: usize = 0;\n    for row in rows {\n        total += *row as usize;\n    }\n    total + rows.len()\n}\n";
        assert_eq!(
            plan_extraction(Lang::Rust, source.as_bytes(), 2, 4, "accumulate"),
            Err(ExtractionRefusal::MovedNameUsedAfterRange("rows".to_string()))
        );
    }

    #[test]
    fn refuses_an_unannotated_local() {
        // Idiomatic Rust rarely annotates a local, which is exactly why this
        // refuses rather than guessing: there is no type checker behind it,
        // and a guessed `T` parses and does not build.
        let source = "fn outer(base: u32) -> u32 {\n    let mut acc = 0;\n    acc += base;\n    acc\n}\n";
        assert_eq!(
            plan_extraction(Lang::Rust, source.as_bytes(), 2, 2, "bump"),
            Err(ExtractionRefusal::UnspellableParameterType("acc".to_string()))
        );
    }

    #[test]
    fn refuses_the_trailing_expression() {
        // The tail *is* the function's return. Hoisting it would hand the
        // value to the new function and leave the caller returning nothing.
        assert_eq!(
            plan_extraction(Lang::Rust, SOURCE.as_bytes(), 5, 5, "finish"),
            Err(ExtractionRefusal::ReturnsThroughTailExpression)
        );
    }

    #[test]
    fn refuses_the_question_mark_operator() {
        // `?` returns from the *enclosing* function. In a new function whose
        // return type is derived from names, there is nothing for it to return
        // through.
        let source = "fn outer(raw: &str) -> Result<u32, E> {\n    let n: u32 = parse(raw)?;\n    Ok(n)\n}\n";
        assert_eq!(
            plan_extraction(Lang::Rust, source.as_bytes(), 1, 1, "parsed"),
            Err(ExtractionRefusal::ControlFlowEscapes("?"))
        );
    }

    #[test]
    fn refuses_an_await() {
        let source = "async fn outer(id: u32) -> u32 {\n    let n: u32 = fetch(id).await;\n    n\n}\n";
        assert_eq!(
            plan_extraction(Lang::Rust, source.as_bytes(), 1, 1, "fetched"),
            Err(ExtractionRefusal::ControlFlowEscapes(".await"))
        );
    }

    #[test]
    fn refuses_a_method_body_that_names_self() {
        // Python threads `self` through the signature because it is an
        // ordinary name. Rust's is not: the new function would have to become
        // an inherent method, which needs an `impl` target and a receiver form
        // no derivation can choose without types.
        let source = "struct S { scale: u32 }\nimpl S {\n    fn outer(&self, base: u32) -> u32 {\n        let n: u32 = self.scale * base;\n        n\n    }\n}\n";
        assert_eq!(
            plan_extraction(Lang::Rust, source.as_bytes(), 3, 3, "scaled"),
            Err(ExtractionRefusal::ReferencesReceiver("self"))
        );
    }

    #[test]
    fn a_method_extraction_without_self_lands_past_the_impl_block() {
        let source = "struct S;\nimpl S {\n    fn outer(&self, base: u32) -> u32 {\n        let n: u32 = base * 2;\n        n\n    }\n}\n";
        let plan =
            plan_extraction(Lang::Rust, source.as_bytes(), 3, 3, "double").expect("planned");
        assert_eq!(plan.enclosing_indent, "");
        assert_eq!(plan.insert_byte, source.len() - 1);
        let (function, call) = render_extraction(&plan, source, "double");
        assert!(function.contains("\nfn double(base: u32) -> u32 {"), "{function}");
        assert_eq!(call, "        let n = double(base);");
    }
}
