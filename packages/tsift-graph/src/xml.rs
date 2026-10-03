//! Namespace-resolved XML lowering shared by the XML contract dialects
//! (`#xsdcontract`, `#wsdlcontract`).
//!
//! A contract dialect is identified by namespace URI, never by the prefix a
//! document happens to bind it to: `xs:`, `xsd:`, and a default
//! `xmlns="http://www.w3.org/2001/XMLSchema"` are the same vocabulary. This
//! module lowers a tree-sitter-xml tree into a small element tree where every
//! element carries its resolved namespace and its in-scope prefix bindings, so
//! a projection can match elements by `(namespace, local name)` and resolve
//! QName-valued attributes (`type="tns:Order"`) the same way.
//!
//! Spans come straight from tree-sitter nodes, so a projected symbol's byte and
//! line extents agree with the chunker's view of the same file.

use std::rc::Rc;

/// The XML Schema namespace (XSD 1.0 and 1.1 share it).
pub(crate) const XSD_NS: &str = "http://www.w3.org/2001/XMLSchema";

/// Bound on element nesting. Real contracts nest a few dozen levels at most; a
/// pathological document must not overflow the stack.
const MAX_DEPTH: usize = 128;

/// One in-scope namespace binding. `prefix: None` is the default namespace.
#[derive(Debug)]
struct Binding {
    prefix: Option<String>,
    uri: String,
}

/// The namespace bindings in scope at an element, innermost last. Shared with
/// the parent when the element declares nothing new.
#[derive(Debug, Default)]
struct Scope {
    bindings: Vec<Binding>,
}

impl Scope {
    fn lookup(&self, prefix: Option<&str>) -> Option<&str> {
        self.bindings
            .iter()
            .rev()
            .find(|binding| binding.prefix.as_deref() == prefix)
            .map(|binding| binding.uri.as_str())
            // `xmlns=""` undeclares the default namespace.
            .filter(|uri| !uri.is_empty() || prefix.is_some())
            .or(match prefix {
                Some("xml") => Some("http://www.w3.org/XML/1998/namespace"),
                _ => None,
            })
    }
}

/// An attribute as written. Attribute names are kept raw: the contract
/// vocabularies read their own attributes unqualified.
#[derive(Debug)]
pub(crate) struct Attr {
    pub name: String,
    pub value: String,
    pub line: usize,
}

#[derive(Debug)]
pub(crate) struct Element {
    /// Resolved namespace URI; `None` when the element is in no namespace.
    pub ns: Option<String>,
    pub local: String,
    pub attrs: Vec<Attr>,
    pub children: Vec<Element>,
    pub start_byte: usize,
    pub end_byte: usize,
    pub line: usize,
    pub end_line: usize,
    /// Span of the element's content (between start and end tag), if any.
    pub content: Option<(usize, usize)>,
    scope: Rc<Scope>,
}

/// A QName attribute value resolved against an element's scope.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct QName<'a> {
    pub ns: Option<&'a str>,
    pub local: &'a str,
}

impl Element {
    pub fn is(&self, ns: &str, local: &str) -> bool {
        self.local == local && self.ns.as_deref() == Some(ns)
    }

    pub fn in_ns(&self, ns: &str) -> bool {
        self.ns.as_deref() == Some(ns)
    }

    pub fn attr(&self, name: &str) -> Option<&Attr> {
        self.attrs.iter().find(|attr| attr.name == name)
    }

    pub fn attr_value(&self, name: &str) -> Option<&str> {
        self.attr(name).map(|attr| attr.value.as_str())
    }

    /// Resolve a QName value (`tns:Order`, `Order`) in this element's scope.
    /// An unprefixed QName takes the default namespace, as XSD and WSDL both
    /// specify. `None` for a value that is not a QName (a URI, `#any`) or whose
    /// prefix is not bound -- naming a local part under an unknown namespace
    /// would invent an edge.
    pub fn resolve_qname<'a>(&'a self, value: &'a str) -> Option<QName<'a>> {
        let value = value.trim();
        let (prefix, local) = match value.split_once(':') {
            Some((prefix, local)) => (Some(prefix), local),
            None => (None, value),
        };
        if !is_ncname(local) || prefix.is_some_and(|p| !is_ncname(p)) {
            return None;
        }
        let ns = self.scope.lookup(prefix);
        if prefix.is_some() && ns.is_none() {
            return None;
        }
        Some(QName { ns, local })
    }
}

/// A loose NCName check: enough to reject URIs, `#any`, and list values.
fn is_ncname(text: &str) -> bool {
    let mut chars = text.chars();
    match chars.next() {
        Some(first) if first.is_alphabetic() || first == '_' => {}
        _ => return false,
    }
    chars.all(|c| c.is_alphanumeric() || matches!(c, '_' | '-' | '.'))
}

fn node_text<'a>(node: tree_sitter::Node<'_>, source: &'a [u8]) -> &'a str {
    node.utf8_text(source).unwrap_or("")
}

/// Lower the root element of a tree-sitter-xml document. A truncated document
/// still lowers whatever elements the parser recovered.
pub(crate) fn lower(root: tree_sitter::Node<'_>, source: &[u8]) -> Option<Element> {
    let element = root.child_by_field_name("root").or_else(|| {
        let mut cursor = root.walk();
        root.named_children(&mut cursor)
            .find(|child| child.kind() == "element")
    })?;
    lower_element(element, source, &Rc::new(Scope::default()), 0)
}

fn lower_element(
    node: tree_sitter::Node<'_>,
    source: &[u8],
    parent_scope: &Rc<Scope>,
    depth: usize,
) -> Option<Element> {
    if depth > MAX_DEPTH {
        return None;
    }
    let mut cursor = node.walk();
    let children: Vec<_> = node.named_children(&mut cursor).collect();
    let tag = children
        .iter()
        .find(|child| matches!(child.kind(), "STag" | "EmptyElemTag"))?;

    let mut qname = None;
    let mut attrs = Vec::new();
    let mut tag_cursor = tag.walk();
    for part in tag.named_children(&mut tag_cursor) {
        match part.kind() {
            "Name" if qname.is_none() => qname = Some(node_text(part, source).to_string()),
            "Attribute" => {
                let mut attr_cursor = part.walk();
                let mut name = None;
                let mut value = None;
                for piece in part.named_children(&mut attr_cursor) {
                    match piece.kind() {
                        "Name" => name = Some(node_text(piece, source)),
                        "AttValue" => value = Some(attribute_value(node_text(piece, source))),
                        _ => {}
                    }
                }
                if let (Some(name), Some(value)) = (name, value) {
                    attrs.push(Attr {
                        name: name.to_string(),
                        value,
                        line: part.start_position().row,
                    });
                }
            }
            _ => {}
        }
    }
    let qname = qname?;

    let declared: Vec<Binding> = attrs
        .iter()
        .filter_map(|attr| {
            if attr.name == "xmlns" {
                Some(Binding {
                    prefix: None,
                    uri: attr.value.clone(),
                })
            } else {
                attr.name.strip_prefix("xmlns:").map(|prefix| Binding {
                    prefix: Some(prefix.to_string()),
                    uri: attr.value.clone(),
                })
            }
        })
        .collect();
    let scope = if declared.is_empty() {
        Rc::clone(parent_scope)
    } else {
        let mut bindings: Vec<Binding> = parent_scope
            .bindings
            .iter()
            .map(|binding| Binding {
                prefix: binding.prefix.clone(),
                uri: binding.uri.clone(),
            })
            .collect();
        bindings.extend(declared);
        Rc::new(Scope { bindings })
    };

    let (prefix, local) = match qname.split_once(':') {
        Some((prefix, local)) => (Some(prefix), local.to_string()),
        None => (None, qname.clone()),
    };
    let ns = scope.lookup(prefix).map(str::to_string);

    let content = children.iter().find(|child| child.kind() == "content");
    let mut elements = Vec::new();
    if let Some(content) = content {
        let mut content_cursor = content.walk();
        for child in content.named_children(&mut content_cursor) {
            if child.kind() == "element"
                && let Some(element) = lower_element(child, source, &scope, depth + 1)
            {
                elements.push(element);
            }
        }
    }

    Some(Element {
        ns,
        local,
        attrs,
        children: elements,
        start_byte: node.start_byte(),
        end_byte: node.end_byte(),
        line: node.start_position().row,
        end_line: node.end_position().row,
        content: content.map(|content| (content.start_byte(), content.end_byte())),
        scope,
    })
}

/// Strip an `AttValue`'s quotes and decode the predefined and numeric
/// character references.
fn attribute_value(raw: &str) -> String {
    let inner = raw
        .strip_prefix('"')
        .and_then(|s| s.strip_suffix('"'))
        .or_else(|| raw.strip_prefix('\'').and_then(|s| s.strip_suffix('\'')))
        .unwrap_or(raw);
    if !inner.contains('&') {
        return inner.to_string();
    }
    let mut out = String::with_capacity(inner.len());
    let mut rest = inner;
    while let Some(amp) = rest.find('&') {
        out.push_str(&rest[..amp]);
        let after = &rest[amp + 1..];
        let decoded = after.find(';').and_then(|semi| {
            let entity = &after[..semi];
            let ch = match entity {
                "lt" => Some('<'),
                "gt" => Some('>'),
                "amp" => Some('&'),
                "quot" => Some('"'),
                "apos" => Some('\''),
                _ => entity
                    .strip_prefix("#x")
                    .and_then(|hex| u32::from_str_radix(hex, 16).ok())
                    .or_else(|| entity.strip_prefix('#').and_then(|dec| dec.parse().ok()))
                    .and_then(char::from_u32),
            }?;
            Some((ch, semi))
        });
        match decoded {
            Some((ch, semi)) => {
                out.push(ch);
                rest = &after[semi + 1..];
            }
            None => {
                out.push('&');
                rest = after;
            }
        }
    }
    out.push_str(rest);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lower_str(source: &str) -> Element {
        let mut parser = tree_sitter::Parser::new();
        parser
            .set_language(&tree_sitter_xml::LANGUAGE_XML.into())
            .unwrap();
        let tree = parser.parse(source, None).unwrap();
        lower(tree.root_node(), source.as_bytes()).unwrap()
    }

    #[test]
    fn prefixes_resolve_to_namespaces_not_spellings() {
        for source in [
            r#"<xs:schema xmlns:xs="http://www.w3.org/2001/XMLSchema"><xs:element name="a"/></xs:schema>"#,
            r#"<xsd:schema xmlns:xsd="http://www.w3.org/2001/XMLSchema"><xsd:element name="a"/></xsd:schema>"#,
            r#"<schema xmlns="http://www.w3.org/2001/XMLSchema"><element name="a"/></schema>"#,
        ] {
            let root = lower_str(source);
            assert!(root.is(XSD_NS, "schema"), "{source}");
            assert!(root.children[0].is(XSD_NS, "element"), "{source}");
            assert_eq!(root.children[0].attr_value("name"), Some("a"));
        }
        // A look-alike prefix bound to another namespace is not XSD.
        let root = lower_str(r#"<xs:schema xmlns:xs="urn:other"/>"#);
        assert!(!root.is(XSD_NS, "schema"));
    }

    #[test]
    fn qnames_resolve_through_inherited_scope() {
        let root = lower_str(
            r#"<a xmlns="urn:default" xmlns:t="urn:t"><b xmlns:u="urn:u" ref="t:X" other="u:Y" plain="Z" uri="http://x/y" bad="nope:Q"/></a>"#,
        );
        let b = &root.children[0];
        let resolve = |name: &str| {
            b.resolve_qname(b.attr_value(name).unwrap())
                .map(|q| (q.ns.map(str::to_string), q.local.to_string()))
        };
        assert_eq!(resolve("ref"), Some((Some("urn:t".into()), "X".into())));
        assert_eq!(resolve("other"), Some((Some("urn:u".into()), "Y".into())));
        assert_eq!(
            resolve("plain"),
            Some((Some("urn:default".into()), "Z".into()))
        );
        assert_eq!(resolve("uri"), None);
        assert_eq!(resolve("bad"), None);
    }

    #[test]
    fn attribute_values_are_unquoted_and_decoded() {
        assert_eq!(attribute_value("\"a&lt;b&#65;&#x42;\""), "a<bAB");
        assert_eq!(attribute_value("'it''s'"), "it''s");
        assert_eq!(attribute_value("\"a & b\""), "a & b");
    }

    #[test]
    fn spans_and_lines_come_from_the_tree() {
        let source = "<r>\n  <c name=\"x\">\n    <d/>\n  </c>\n</r>\n";
        let root = lower_str(source);
        let c = &root.children[0];
        assert_eq!((c.line, c.end_line), (1, 3));
        assert!(source[c.start_byte..c.end_byte].starts_with("<c name"));
        assert!(source[c.start_byte..c.end_byte].ends_with("</c>"));
        assert_eq!(c.attr("name").unwrap().line, 1);
        let (start, end) = c.content.unwrap();
        assert!(c.start_byte < start && end < c.end_byte);
    }
}
