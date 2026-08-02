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

use crate::lang::Lang;
use std::collections::BTreeSet;
use tree_sitter::{Node, Parser};

/// A derived extraction, ready to be spelled by a language emitter.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExtractionPlan {
    /// The function the range was taken out of.
    pub enclosing_function: String,
    /// Names read in the range before the range assigns them, that are bound in
    /// the enclosing function outside the range. Sorted, so the signature and
    /// the call site cannot disagree about argument order.
    pub parameters: Vec<String>,
    /// Names assigned in the range and read after it, in the same order rule.
    pub returns: Vec<String>,
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
    /// Control flow leaves the range: hoisting it changes what it does, and no
    /// signature can carry that.
    ControlFlowEscapes(&'static str),
    /// The range assigns a name the enclosing function declared `global` or
    /// `nonlocal`; the assignment's effect is outside the new function's scope.
    RebindsOuterScope(String),
    /// The extracted name already binds something visible at the call site.
    NameCollision(String),
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
            Self::ControlFlowEscapes(kind) => {
                format!("the range contains `{kind}`, whose effect leaves the extracted function")
            }
            Self::RebindsOuterScope(name) => {
                format!("the range assigns `{name}`, which the enclosing function declares global or nonlocal")
            }
            Self::NameCollision(name) => {
                format!("`{name}` already binds a value visible at the call site")
            }
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
    if !matches!(lang, l if is_python(l)) {
        return Err(ExtractionRefusal::UnsupportedLanguage(lang.name()));
    }
    let ts_lang = lang.tree_sitter_language();
    let mut parser = Parser::new();
    parser
        .set_language(&ts_lang)
        .map_err(|_| ExtractionRefusal::ParseFailed)?;
    let tree = parser
        .parse(source, None)
        .ok_or(ExtractionRefusal::ParseFailed)?;
    let root = tree.root_node();

    let selection = select_sibling_run(root, start_line, end_line)?;
    let block = selection[0]
        .parent()
        .ok_or(ExtractionRefusal::NotContiguousSiblings)?;
    let function = enclosing_function(block).ok_or(ExtractionRefusal::NotInsideFunction)?;

    for statement in &selection {
        if let Some(kind) = escaping_control_flow(*statement) {
            return Err(ExtractionRefusal::ControlFlowEscapes(kind));
        }
    }

    let start_byte = selection[0].start_byte();
    let end_byte = selection[selection.len() - 1].end_byte();

    let assigned_in_range = names_bound_in(&selection, source);
    let scope_pinned = scope_pinned_names(function, source);
    if let Some(name) = assigned_in_range.intersection(&scope_pinned).next() {
        return Err(ExtractionRefusal::RebindsOuterScope(name.clone()));
    }

    let read_first_in_range = names_read_before_assignment(&selection, source);
    let bound_outside_range = names_bound_in_function_outside(function, start_byte, end_byte, source);
    let read_after_range = names_read_after(function, end_byte, source);
    let module_scope = module_scope_names(root, source);

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

    Ok(ExtractionPlan {
        enclosing_function: function
            .child_by_field_name("name")
            .and_then(|name| name.utf8_text(source).ok())
            .unwrap_or_default()
            .to_string(),
        parameters,
        returns,
        start_byte,
        end_byte,
        indent: line_indent(source, start_byte),
        insert_byte: function.end_byte(),
        enclosing_indent: line_indent(source, function.start_byte()),
    })
}

/// Render a Python extraction: the new `def` and the call that replaces the
/// range, both already indented for their positions.
pub fn render_python_extraction(
    plan: &ExtractionPlan,
    source: &str,
    new_name: &str,
) -> (String, String) {
    let body = &source[plan.start_byte..plan.end_byte];
    let inner_indent = format!("{}    ", plan.enclosing_indent);
    let mut rendered = String::new();
    for (index, line) in body.lines().enumerate() {
        if index > 0 {
            rendered.push('\n');
        }
        if line.trim().is_empty() {
            continue;
        }
        let stripped = line.strip_prefix(&plan.indent).unwrap_or(line);
        rendered.push_str(&inner_indent);
        rendered.push_str(stripped);
    }

    let mut function = format!(
        "\n\n{}def {new_name}({}):\n{rendered}",
        plan.enclosing_indent,
        plan.parameters.join(", ")
    );
    if !plan.returns.is_empty() {
        function.push('\n');
        function.push_str(&inner_indent);
        function.push_str("return ");
        function.push_str(&plan.returns.join(", "));
    }
    function.push('\n');

    let call_expression = format!("{new_name}({})", plan.parameters.join(", "));
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

#[cfg(feature = "lang-python")]
fn is_python(lang: Lang) -> bool {
    matches!(lang, Lang::Python)
}

#[cfg(not(feature = "lang-python"))]
fn is_python(_lang: Lang) -> bool {
    false
}

/// The statements that start inside the requested lines, verified to be a
/// contiguous run of siblings in one block.
fn select_sibling_run(
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
                && node
                    .parent()
                    .is_some_and(|parent| parent.kind() == "block" || parent.kind() == "module")
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

fn enclosing_function(node: Node) -> Option<Node> {
    let mut current = node.parent();
    while let Some(candidate) = current {
        if candidate.kind() == "function_definition" {
            return Some(candidate);
        }
        current = candidate.parent();
    }
    None
}

/// A statement whose effect is defined by the block it sits in, and therefore
/// cannot move into another function.
fn escaping_control_flow(statement: Node) -> Option<&'static str> {
    let mut found = None;
    walk(statement, &mut |node| {
        if found.is_some() {
            return false;
        }
        // A nested function or lambda re-scopes these, so its body is not part
        // of the range's control flow.
        if node.id() != statement.id()
            && matches!(node.kind(), "function_definition" | "lambda")
        {
            return false;
        }
        found = match node.kind() {
            "return_statement" => Some("return"),
            "break_statement" => Some("break"),
            "continue_statement" => Some("continue"),
            "yield" => Some("yield"),
            _ => None,
        };
        found.is_none()
    });
    found
}

/// Names the enclosing function pinned to an outer scope.
fn scope_pinned_names(function: Node, source: &[u8]) -> BTreeSet<String> {
    let mut names = BTreeSet::new();
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

fn names_bound_in(statements: &[Node], source: &[u8]) -> BTreeSet<String> {
    let mut names = BTreeSet::new();
    for statement in statements {
        walk(*statement, &mut |node| {
            if let Some(name) = binding_name(node, source) {
                names.insert(name);
            }
            true
        });
    }
    names
}

fn names_bound_in_function_outside(
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
        if let Some(name) = binding_name(node, source) {
            names.insert(name);
        }
        true
    });
    // The function's own parameters bind for the whole body.
    if let Some(parameters) = function.child_by_field_name("parameters") {
        walk(parameters, &mut |node| {
            if node.kind() == "identifier"
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
fn names_read_before_assignment(statements: &[Node], source: &[u8]) -> BTreeSet<String> {
    let mut assigned: BTreeSet<String> = BTreeSet::new();
    let mut read_first: BTreeSet<String> = BTreeSet::new();
    let mut events: Vec<(usize, bool, String)> = Vec::new();
    for statement in statements {
        walk(*statement, &mut |node| {
            if let Some(name) = binding_name(node, source) {
                // An augmented assignment reads its target before writing it.
                if node
                    .parent()
                    .is_some_and(|parent| parent.kind() == "augmented_assignment")
                {
                    events.push((node.start_byte(), false, name.clone()));
                }
                events.push((node.start_byte(), true, name));
                return true;
            }
            if is_read_identifier(node)
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

fn names_read_after(function: Node, end_byte: usize, source: &[u8]) -> BTreeSet<String> {
    let mut names = BTreeSet::new();
    walk(function, &mut |node| {
        if node.end_byte() <= end_byte {
            return node.end_byte() > end_byte || node.child_count() > 0;
        }
        if is_read_identifier(node)
            && node.start_byte() >= end_byte
            && let Ok(text) = node.utf8_text(source)
        {
            names.insert(text.to_string());
        }
        true
    });
    names
}

fn module_scope_names(root: Node, source: &[u8]) -> BTreeSet<String> {
    let mut names = BTreeSet::new();
    let mut cursor = root.walk();
    for statement in root.named_children(&mut cursor) {
        match statement.kind() {
            "function_definition" | "class_definition" => {
                if let Some(name) = statement
                    .child_by_field_name("name")
                    .and_then(|name| name.utf8_text(source).ok())
                {
                    names.insert(name.to_string());
                }
            }
            _ => {
                walk(statement, &mut |node| {
                    if node.kind() == "function_definition" || node.kind() == "class_definition" {
                        return false;
                    }
                    if let Some(name) = binding_name(node, source) {
                        names.insert(name);
                    }
                    true
                });
            }
        }
    }
    names
}

/// The name this node binds, if it is a binding position.
fn binding_name(node: Node, source: &[u8]) -> Option<String> {
    if node.kind() != "identifier" {
        return None;
    }
    let parent = node.parent()?;
    let is_binding = match parent.kind() {
        "assignment" | "augmented_assignment" | "for_statement" => parent
            .child_by_field_name("left")
            .is_some_and(|left| covers(left, node)),
        "pattern_list" | "tuple_pattern" | "list_pattern" => parent
            .parent()
            .is_some_and(|grand| matches!(grand.kind(), "assignment" | "for_statement")),
        "as_pattern_target" | "aliased_import" => true,
        "function_definition" | "class_definition" => parent
            .child_by_field_name("name")
            .is_some_and(|name| name.id() == node.id()),
        _ => false,
    };
    if !is_binding {
        return None;
    }
    node.utf8_text(source).ok().map(str::to_string)
}

fn covers(outer: Node, inner: Node) -> bool {
    outer.id() == inner.id()
        || (outer.start_byte() <= inner.start_byte() && outer.end_byte() >= inner.end_byte())
}

/// Whether this identifier reads a binding, as opposed to naming a member, a
/// keyword argument, or a binding position.
fn is_read_identifier(node: Node) -> bool {
    if node.kind() != "identifier" {
        return false;
    }
    let Some(parent) = node.parent() else {
        return false;
    };
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
        _ => true,
    }
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
mod tests {
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
        assert_eq!(plan.parameters, vec!["prefix".to_string(), "scale".to_string()]);
        assert_eq!(plan.returns, vec!["acc".to_string()]);
        assert_eq!(plan.indent, "    ");
    }

    #[test]
    fn a_name_the_range_assigns_first_is_a_local_not_a_parameter() {
        // `acc` is bound *outside* the range on line 1, so the outer-binding
        // test alone would make it a parameter. It is not one: line 2 assigns
        // it before line 3 reads it, so passing the outer value in would feed
        // the extracted body a value it immediately overwrites, and the caller
        // would keep computing an argument nothing reads.
        let source = "def outer(base):\n    acc = 0\n    acc = base * 2\n    acc += 1\n    return acc\n";
        let plan = plan_extraction(Lang::Python, source.as_bytes(), 2, 3, "recompute")
            .expect("planned");

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
    fn refuses_a_range_whose_control_flow_escapes() {
        assert_eq!(
            plan(9, 9, "finish"),
            Err(ExtractionRefusal::ControlFlowEscapes("return"))
        );
    }

    #[test]
    fn refuses_a_range_outside_any_function() {
        assert_eq!(plan(0, 0, "setup"), Err(ExtractionRefusal::NotInsideFunction));
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
        let source = "COUNT = 0\n\n\ndef outer():\n    global COUNT\n    COUNT = 1\n    return COUNT\n";
        assert_eq!(
            plan_extraction(Lang::Python, source.as_bytes(), 5, 5, "bump"),
            Err(ExtractionRefusal::RebindsOuterScope("COUNT".to_string()))
        );
    }

    #[test]
    fn renders_a_def_and_a_destructuring_call_that_agree() {
        let plan = plan(5, 7, "accumulate").expect("planned");
        let (function, call) = render_python_extraction(&plan, SOURCE, "accumulate");

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
        let plan = plan_extraction(Lang::Python, source.as_bytes(), 2, 2, "report").expect("planned");
        assert!(plan.returns.is_empty(), "{:?}", plan.returns);
        let (function, call) = render_python_extraction(&plan, source, "report");
        assert!(function.contains("def report(scale):"), "{function}");
        assert!(!function.contains("return"), "{function}");
        assert_eq!(call, "    report(scale)");
    }
}
