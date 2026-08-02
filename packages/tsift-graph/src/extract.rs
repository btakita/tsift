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
            Self::AssignsUndeclaredName(name) => {
                format!("the range assigns `{name}` without declaring it, and `{name}` is not a local of the enclosing function")
            }
            Self::UnspellableParameterType(name) => {
                format!("`{name}` has no annotation to copy, and this language will not take an unannotated parameter")
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
        }
    }

    fn is_block_kind(self, kind: &str) -> bool {
        match self.family {
            Family::Python => kind == "block",
            Family::GdScript => kind == "body",
            Family::JsLike => kind == "statement_block",
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
            }
    }

    fn is_class_kind(self, kind: &str) -> bool {
        match self.family {
            Family::Python | Family::GdScript => kind == "class_definition",
            Family::JsLike => matches!(kind, "class_declaration" | "class" | "class_body"),
        }
    }

    /// Kinds whose effect is defined by the block they sit in.
    fn escaping_kind(self, kind: &str) -> Option<&'static str> {
        let escape = match (self.family, kind) {
            (_, "return_statement") => "return",
            (_, "break_statement") => "break",
            (_, "continue_statement") => "continue",
            (Family::Python, "yield") => "yield",
            (Family::JsLike, "yield_expression") => "yield",
            // `throw`/`raise` is deliberately absent: an exception propagates
            // through a call frame unchanged, so hoisting it does not move
            // where it is caught.
            _ => return None,
        };
        Some(escape)
    }

    /// Kinds that group several binding positions into one target.
    fn pattern_kinds(self) -> &'static [&'static str] {
        match self.family {
            Family::Python => &["pattern_list", "tuple_pattern", "list_pattern"],
            Family::GdScript => &[],
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
            Family::JsLike => Some("let"),
        }
    }

    fn default_indent_unit(self) -> &'static str {
        match self.family {
            Family::Python => "    ",
            Family::GdScript => "\t",
            Family::JsLike => "  ",
        }
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
    let function = hoistable_enclosing_function(dialect, block)?;

    for statement in &selection {
        if let Some(kind) = escaping_control_flow(dialect, *statement) {
            return Err(ExtractionRefusal::ControlFlowEscapes(kind));
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
    let module_scope = module_scope_names(dialect, root, source);

    if bound_outside_range.contains(new_name) || module_scope.contains(new_name) {
        return Err(ExtractionRefusal::NameCollision(new_name.to_string()));
    }

    let parameters = read_first_in_range
        .intersection(&bound_outside_range)
        .cloned()
        .collect::<Vec<_>>();
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

    let mut parameter_spellings = Vec::with_capacity(parameters.len());
    for parameter in &parameters {
        parameter_spellings.push(spell_parameter(dialect, function, parameter, source)?);
    }

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
        local_declarations,
        returns_need_declaration,
        start_byte,
        end_byte,
        indent: line_indent(source, start_byte),
        insert_byte: function.end_byte(),
        enclosing_indent: line_indent(source, function.start_byte()),
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
    let terminator = if dialect.family == Family::JsLike {
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
    dialect
        .declaration_keyword()
        .map(|keyword| format!("{keyword} "))
        .unwrap_or_default()
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
    source: &[u8],
) -> Result<String, ExtractionRefusal> {
    if !dialect.annotates_parameters {
        return Ok(name.to_string());
    }
    match existing_type_annotation(function, name, source) {
        Some(annotation) => Ok(format!("{name}{annotation}")),
        None => Err(ExtractionRefusal::UnspellableParameterType(
            name.to_string(),
        )),
    }
}

/// The annotation text (`": number"`) attached to `name`'s binding inside the
/// enclosing function, if it has one.
fn existing_type_annotation(function: Node, name: &str, source: &[u8]) -> Option<String> {
    let mut found = None;
    walk(function, &mut |node| {
        if found.is_some() {
            return false;
        }
        let binder = match node.kind() {
            "required_parameter" | "optional_parameter" => node.child_by_field_name("pattern"),
            "variable_declarator" => node.child_by_field_name("name"),
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
fn hoistable_enclosing_function(
    dialect: Dialect,
    block: Node,
) -> Result<Node, ExtractionRefusal> {
    let mut current = Some(block);
    while let Some(candidate) = current {
        if dialect.is_function_kind(candidate.kind()) {
            let parent = candidate
                .parent()
                .ok_or(ExtractionRefusal::EnclosingFunctionNotHoistable)?;
            if !dialect.is_block_kind(parent.kind()) && parent.kind() != dialect.root_kind() {
                return Err(ExtractionRefusal::EnclosingFunctionNotHoistable);
            }
            if parent
                .parent()
                .is_some_and(|grand| dialect.is_class_kind(grand.kind()))
            {
                return Err(ExtractionRefusal::EnclosingFunctionNotHoistable);
            }
            return Ok(candidate);
        }
        current = candidate.parent();
    }
    Err(ExtractionRefusal::NotInsideFunction)
}

/// A statement whose effect is defined by the block it sits in, and therefore
/// cannot move into another function.
fn escaping_control_flow(dialect: Dialect, statement: Node) -> Option<&'static str> {
    let mut found = None;
    walk(statement, &mut |node| {
        if found.is_some() {
            return false;
        }
        // A nested function or lambda re-scopes these, so its body is not part
        // of the range's control flow.
        if node.id() != statement.id() && dialect.is_nested_scope_kind(node.kind()) {
            return false;
        }
        found = dialect.escaping_kind(node.kind());
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

fn module_scope_names(dialect: Dialect, root: Node, source: &[u8]) -> BTreeSet<String> {
    let mut names = BTreeSet::new();
    let mut cursor = root.walk();
    for statement in root.named_children(&mut cursor) {
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
    }
}

fn is_augmented_assignment(dialect: Dialect, kind: &str) -> bool {
    match dialect.family {
        Family::Python | Family::GdScript => kind == "augmented_assignment",
        Family::JsLike => kind == "augmented_assignment_expression",
    }
}

/// Whether this node names something at all in this dialect, ignoring whether
/// the position reads or binds it.
fn is_name_node(dialect: Dialect, node: Node) -> bool {
    match dialect.family {
        Family::Python => node.kind() == "identifier",
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
    fn refuses_to_hoist_out_of_a_method() {
        // A `def` placed beside a method becomes another method, and the bare
        // call left in its place does not resolve to it. Refusing is the whole
        // point: the alternative parses, formats, and raises at run time.
        let source = "class Panel:\n    def outer(self, base):\n        acc = base * 2\n        return acc\n";
        assert_eq!(
            plan_extraction(Lang::Python, source.as_bytes(), 2, 2, "double"),
            Err(ExtractionRefusal::EnclosingFunctionNotHoistable)
        );
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
    fn refuses_to_hoist_out_of_an_inner_class() {
        let source = "class Panel:\n\tfunc outer(base):\n\t\tvar acc = base * 2\n\t\treturn acc\n";
        assert_eq!(
            plan_extraction(Lang::GdScript, source.as_bytes(), 2, 2, "double"),
            Err(ExtractionRefusal::EnclosingFunctionNotHoistable)
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
    fn refuses_to_hoist_out_of_a_method() {
        let source = "class Panel {\n  outer(base) {\n    let acc = base * 2;\n    return acc;\n  }\n}\n";
        assert_eq!(
            plan_extraction(Lang::JavaScript, source.as_bytes(), 2, 2, "double"),
            Err(ExtractionRefusal::EnclosingFunctionNotHoistable)
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
mod unsupported_tests {
    use super::*;

    #[test]
    fn a_language_outside_the_untyped_family_is_refused_by_name() {
        let source = "fn outer(base: i32) -> i32 {\n    let acc = base * 2;\n    acc\n}\n";
        assert_eq!(
            plan_extraction(Lang::Rust, source.as_bytes(), 1, 1, "double"),
            Err(ExtractionRefusal::UnsupportedLanguage("rust"))
        );
    }
}
