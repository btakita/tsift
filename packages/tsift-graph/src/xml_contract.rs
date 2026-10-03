//! Structural projection of XML API contracts (`#xsdcontract`,
//! `#wsdlcontract`).
//!
//! The XML counterpart of `crate::contract`: an XML Schema or WSDL document is
//! lowered through `crate::xml` (namespace-resolved, so `xs:`, `xsd:`, and a
//! default namespace read alike) and projected into the nodes a reader
//! navigates by. QName references (`type=`, `ref=`, `base=`, `message=`, ...)
//! become call sites naming the referenced component's local part, so `graph
//! --callers` on a type lists the declarations that use it. A WSDL's embedded
//! `<types>` schemas go through the same XSD projection.

use crate::xml::{self, Element, XSD_NS};
use crate::{CallSite, Lang, Symbol};

/// Bound on projection recursion, matching the lowering's own bound.
const MAX_DEPTH: usize = 128;

/// Symbols and reference sites of one contract document.
#[derive(Debug, Default)]
pub(crate) struct Projection {
    pub symbols: Vec<Symbol>,
    pub sites: Vec<CallSite>,
}

impl Projection {
    fn emit(&mut self, element: &Element, name: String, kind: &str) {
        if name.is_empty() {
            return;
        }
        let (body_start_byte, body_end_byte) = match element.content {
            Some((start, end)) => (Some(start), Some(end)),
            None => (None, None),
        };
        self.symbols.push(Symbol {
            name,
            kind: kind.to_string(),
            line: element.line,
            end_line: element.end_line,
            // The vocabulary element (`complexType`, `portType`, ...), so a
            // reader can tell an element declaration from a type of the same
            // name without a second kind per construct.
            node_kind: element.local.clone(),
            start_byte: element.start_byte,
            end_byte: element.end_byte,
            body_start_byte,
            body_end_byte,
        });
    }

    /// A QName reference on `element`, as a call site naming its local part.
    /// References into the XML Schema namespace are built-in types
    /// (`xs:string`), never project symbols, and yield no site.
    fn reference(&mut self, element: &Element, attr: &str) {
        let Some(attr) = element.attr(attr) else {
            return;
        };
        for value in attr.value.split_whitespace() {
            let Some(qname) = element.resolve_qname(value) else {
                continue;
            };
            if qname.ns == Some(XSD_NS) {
                continue;
            }
            self.sites.push(CallSite {
                callee: qname.local.to_string(),
                line: attr.line,
            });
        }
    }

    fn finish(mut self) -> Self {
        self.symbols
            .sort_by_key(|symbol| (symbol.line, symbol.start_byte));
        self.sites.sort_by_key(|site| site.line);
        self
    }
}

/// Project a parsed XML contract for `lang`; `None` when `lang` is not an XML
/// contract format. A document whose root is not the dialect's root element
/// projects to nothing.
pub(crate) fn project(lang: Lang, tree: &tree_sitter::Tree, source: &[u8]) -> Option<Projection> {
    if !lang.is_xml_contract() {
        return None;
    }
    let mut projection = Projection::default();
    if let Some(root) = xml::lower(tree.root_node(), source) {
        match lang {
            #[cfg(feature = "lang-wsdl")]
            Lang::Wsdl => {
                if root.is(WSDL11_NS, "definitions") {
                    wsdl(&root, WSDL11_NS, &mut projection);
                } else if root.is(WSDL20_NS, "description") {
                    wsdl(&root, WSDL20_NS, &mut projection);
                }
            }
            _ => {
                if root.is(XSD_NS, "schema") {
                    xsd_schema(&root, &mut projection);
                }
            }
        }
    }
    Some(projection.finish())
}

/// Top-level XSD components that are named declarations.
const XSD_COMPONENTS: &[&str] = &[
    "complexType",
    "simpleType",
    "element",
    "attribute",
    "group",
    "attributeGroup",
];

/// Attributes whose value is one QName naming another schema component.
const XSD_QNAME_ATTRS: &[&str] = &["type", "ref", "base", "substitutionGroup", "itemType"];

/// Project one `xs:schema` element: its named top-level components as
/// `schema` symbols, their local elements and attributes as `Owner.child`
/// properties, and every QName reference in it as a call site.
pub(crate) fn xsd_schema(schema: &Element, out: &mut Projection) {
    xsd_top_level(schema, out);
    xsd_references(schema, out, 0);
}

fn xsd_top_level(container: &Element, out: &mut Projection) {
    for child in &container.children {
        if !child.in_ns(XSD_NS) {
            continue;
        }
        match child.local.as_str() {
            // Redefined/overridden components are top-level declarations too.
            "redefine" | "override" => xsd_top_level(child, out),
            local if XSD_COMPONENTS.contains(&local) => {
                let Some(name) = child.attr_value("name") else {
                    continue;
                };
                out.emit(child, name.to_string(), "schema");
                xsd_members(child, name, out, 0);
            }
            _ => {}
        }
    }
}

/// Walk a component's content model for the elements and attributes it
/// declares, through compositors, anonymous types, and derivations.
fn xsd_members(parent: &Element, owner: &str, out: &mut Projection, depth: usize) {
    if depth > MAX_DEPTH {
        return;
    }
    for child in &parent.children {
        if !child.in_ns(XSD_NS) {
            continue;
        }
        match child.local.as_str() {
            "element" | "attribute" => {
                // A `ref=` particle still adds a child named by its target.
                let member = child.attr_value("name").or_else(|| {
                    child
                        .attr_value("ref")
                        .and_then(|value| child.resolve_qname(value))
                        .map(|qname| qname.local)
                });
                let Some(member) = member else {
                    continue;
                };
                let name = format!("{owner}.{member}");
                out.emit(child, name.clone(), "property");
                xsd_members(child, &name, out, depth + 1);
            }
            "complexType" | "simpleType" | "sequence" | "choice" | "all" | "complexContent"
            | "simpleContent" | "extension" | "restriction" => {
                xsd_members(child, owner, out, depth + 1)
            }
            _ => {}
        }
    }
}

fn xsd_references(element: &Element, out: &mut Projection, depth: usize) {
    if depth > MAX_DEPTH {
        return;
    }
    for child in &element.children {
        // Documentation and foreign-namespace extensions carry no references.
        if !child.in_ns(XSD_NS) || child.local == "annotation" {
            continue;
        }
        for attr in XSD_QNAME_ATTRS {
            out.reference(child, attr);
        }
        // `xs:union memberTypes` is a whitespace-separated QName list.
        out.reference(child, "memberTypes");
        xsd_references(child, out, depth + 1);
    }
}

/// The WSDL 1.1 namespace.
#[cfg(feature = "lang-wsdl")]
pub(crate) const WSDL11_NS: &str = "http://schemas.xmlsoap.org/wsdl/";
/// The WSDL 2.0 namespace.
#[cfg(feature = "lang-wsdl")]
pub(crate) const WSDL20_NS: &str = "http://www.w3.org/ns/wsdl";

/// Attributes on WSDL elements whose value is a QName naming another
/// component: a message (1.1 `input`/`output`/`fault`), a schema element or
/// type (1.1 `part`, 2.0 message references), a binding (`port`/`endpoint`),
/// a port type (1.1 `binding type=`), or an interface (2.0 `binding`/`service`).
#[cfg(feature = "lang-wsdl")]
const WSDL_QNAME_ATTRS: &[&str] = &["message", "element", "type", "binding", "interface"];

/// Project a WSDL 1.1 `definitions` or 2.0 `description` root. Schemas
/// embedded under `types` go through the XSD projection, so a type defined
/// inline in a WSDL is a symbol like one in a standalone `.xsd`.
#[cfg(feature = "lang-wsdl")]
fn wsdl(root: &Element, ns: &str, out: &mut Projection) {
    for child in &root.children {
        if !child.in_ns(ns) {
            continue;
        }
        let name = child.attr_value("name");
        match (child.local.as_str(), name) {
            ("types", _) => {
                for schema in &child.children {
                    if schema.is(XSD_NS, "schema") {
                        xsd_schema(schema, out);
                    }
                }
                continue;
            }
            ("message", Some(name)) => {
                out.emit(child, name.to_string(), "message");
                wsdl_members(child, ns, name, &["part"], "property", out);
            }
            ("portType" | "interface", Some(name)) => {
                out.emit(child, name.to_string(), "interface");
                wsdl_members(child, ns, name, &["operation"], "operation", out);
            }
            ("binding", Some(name)) => {
                out.emit(child, name.to_string(), "binding");
                wsdl_members(child, ns, name, &["operation"], "operation", out);
            }
            ("service", Some(name)) => {
                out.emit(child, name.to_string(), "service");
                wsdl_members(child, ns, name, &["port", "endpoint"], "endpoint", out);
            }
            _ => {}
        }
        wsdl_references(child, ns, out, 0);
    }
}

/// The named children of a WSDL component, as `Owner.child` symbols.
#[cfg(feature = "lang-wsdl")]
fn wsdl_members(
    parent: &Element,
    ns: &str,
    owner: &str,
    locals: &[&str],
    kind: &str,
    out: &mut Projection,
) {
    for child in &parent.children {
        if child.in_ns(ns)
            && locals.contains(&child.local.as_str())
            && let Some(name) = child.attr_value("name")
        {
            out.emit(child, format!("{owner}.{name}"), kind);
        }
    }
}

/// QName references on a WSDL element and its WSDL descendants. Extension
/// elements (`soap:binding`, `soap:address`) are another vocabulary and
/// carry none of these.
#[cfg(feature = "lang-wsdl")]
fn wsdl_references(element: &Element, ns: &str, out: &mut Projection, depth: usize) {
    if depth > MAX_DEPTH || element.local == "documentation" {
        return;
    }
    for attr in WSDL_QNAME_ATTRS {
        out.reference(element, attr);
    }
    for child in &element.children {
        if child.in_ns(ns) {
            wsdl_references(child, ns, out, depth + 1);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn xsd(source: &str) -> Projection {
        let lang = Lang::Xsd;
        let mut parser = tree_sitter::Parser::new();
        parser.set_language(&lang.tree_sitter_language()).unwrap();
        let tree = parser.parse(source, None).unwrap();
        project(lang, &tree, source.as_bytes()).unwrap()
    }

    fn names(symbols: &[Symbol]) -> Vec<(String, String)> {
        symbols
            .iter()
            .map(|s| (s.kind.clone(), s.name.clone()))
            .collect()
    }

    fn pair(kind: &str, name: &str) -> (String, String) {
        (kind.to_string(), name.to_string())
    }

    const ORDER_XSD: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<xs:schema xmlns:xs="http://www.w3.org/2001/XMLSchema"
           xmlns:tns="urn:example:orders"
           targetNamespace="urn:example:orders">
  <xs:element name="Order" type="tns:OrderType"/>
  <xs:complexType name="OrderType">
    <xs:annotation><xs:documentation>An order.</xs:documentation></xs:annotation>
    <xs:sequence>
      <xs:element name="id" type="xs:string"/>
      <xs:element name="shipTo" type="tns:Address"/>
      <xs:element ref="tns:Note" minOccurs="0"/>
    </xs:sequence>
    <xs:attribute name="status" type="tns:Status"/>
  </xs:complexType>
  <xs:complexType name="Address">
    <xs:sequence>
      <xs:element name="street" type="xs:string"/>
    </xs:sequence>
  </xs:complexType>
  <xs:complexType name="UsAddress">
    <xs:complexContent>
      <xs:extension base="tns:Address">
        <xs:sequence><xs:element name="zip" type="xs:string"/></xs:sequence>
      </xs:extension>
    </xs:complexContent>
  </xs:complexType>
  <xs:simpleType name="Status">
    <xs:restriction base="xs:string"/>
  </xs:simpleType>
  <xs:element name="Note">
    <xs:complexType>
      <xs:sequence><xs:element name="text" type="xs:string"/></xs:sequence>
    </xs:complexType>
  </xs:element>
  <xs:element name="Memo" substitutionGroup="tns:Note"/>
  <xs:group name="Lines"><xs:sequence><xs:element name="line" type="xs:int"/></xs:sequence></xs:group>
  <xs:attributeGroup name="Audit"><xs:attribute name="by" type="xs:string"/></xs:attributeGroup>
</xs:schema>
"#;

    #[test]
    fn xsd_projects_components_and_owner_properties() {
        let projection = xsd(ORDER_XSD);
        assert_eq!(
            names(&projection.symbols),
            vec![
                pair("schema", "Order"),
                pair("schema", "OrderType"),
                pair("property", "OrderType.id"),
                pair("property", "OrderType.shipTo"),
                pair("property", "OrderType.Note"),
                pair("property", "OrderType.status"),
                pair("schema", "Address"),
                pair("property", "Address.street"),
                pair("schema", "UsAddress"),
                pair("property", "UsAddress.zip"),
                pair("schema", "Status"),
                pair("schema", "Note"),
                pair("property", "Note.text"),
                pair("schema", "Memo"),
                pair("schema", "Lines"),
                pair("property", "Lines.line"),
                pair("schema", "Audit"),
                pair("property", "Audit.by"),
            ]
        );
        let order_type = &projection.symbols[1];
        assert_eq!(order_type.node_kind, "complexType");
        assert_eq!(order_type.line, 5);
        assert_eq!(order_type.end_line, 13);
        let span = &ORDER_XSD[order_type.start_byte..order_type.end_byte];
        assert!(span.starts_with("<xs:complexType name=\"OrderType\">"));
        assert!(span.ends_with("</xs:complexType>"));
        let (body_start, body_end) = (
            order_type.body_start_byte.unwrap(),
            order_type.body_end_byte.unwrap(),
        );
        assert!(order_type.start_byte < body_start && body_end < order_type.end_byte);
    }

    #[test]
    fn xsd_references_strip_prefixes_and_skip_builtins() {
        let projection = xsd(ORDER_XSD);
        let sites: Vec<(&str, usize)> = projection
            .sites
            .iter()
            .map(|site| (site.callee.as_str(), site.line))
            .collect();
        assert_eq!(
            sites,
            vec![
                ("OrderType", 4),
                ("Address", 9),
                ("Note", 10),
                ("Status", 12),
                ("Address", 21),
                ("Note", 34),
            ]
        );
    }

    /// The default-namespace spelling projects identically: unprefixed
    /// `type="string"` is a built-in, and the schema's own types take a prefix.
    #[test]
    fn default_namespace_schema_reads_like_a_prefixed_one() {
        let source = r#"<schema xmlns="http://www.w3.org/2001/XMLSchema" xmlns:t="urn:t" targetNamespace="urn:t">
  <complexType name="A"><sequence><element name="b" type="t:B"/><element name="c" type="string"/></sequence></complexType>
  <simpleType name="B"><list itemType="int"/></simpleType>
  <simpleType name="C"><union memberTypes="t:B string t:A"/></simpleType>
</schema>"#;
        let projection = xsd(source);
        assert_eq!(
            names(&projection.symbols),
            vec![
                pair("schema", "A"),
                pair("property", "A.b"),
                pair("property", "A.c"),
                pair("schema", "B"),
                pair("schema", "C"),
            ]
        );
        let callees: Vec<&str> = projection
            .sites
            .iter()
            .map(|site| site.callee.as_str())
            .collect();
        assert_eq!(callees, vec!["B", "B", "A"]);
    }

    #[test]
    fn non_schema_xml_projects_nothing() {
        let projection =
            xsd(r#"<project xmlns:xs="urn:not-xsd"><xs:element name="a" type="b"/></project>"#);
        assert!(projection.symbols.is_empty());
        assert!(projection.sites.is_empty());
    }

    /// Through the public entry points the indexer uses: `graph --callers` on a
    /// type lists the declarations that name it via `type=` and `base=`.
    #[test]
    fn type_and_base_references_resolve_to_caller_edges() {
        let lang = Lang::Xsd;
        let source = ORDER_XSD.as_bytes();
        let symbols = lang.extract_symbols(source).unwrap();
        let sites = crate::extract_call_sites(lang, source).unwrap();
        let edges: Vec<(String, String)> = crate::resolve_edges(&symbols, &sites)
            .into_iter()
            .map(|edge| (edge.caller, edge.callee))
            .collect();
        let mut address_callers: Vec<&str> = edges
            .iter()
            .filter(|(_, callee)| callee == "Address")
            .map(|(caller, _)| caller.as_str())
            .collect();
        address_callers.sort_unstable();
        // `OrderType.shipTo type=` (a property: attributed to its owner) and
        // `UsAddress`'s `extension base=`.
        assert_eq!(address_callers, vec!["OrderType", "UsAddress"]);
        assert!(edges.contains(&("Order".to_string(), "OrderType".to_string())));
        assert!(edges.contains(&("Memo".to_string(), "Note".to_string())));
    }

    #[cfg(feature = "lang-wsdl")]
    fn wsdl_projection(source: &str) -> Projection {
        let lang = Lang::Wsdl;
        let mut parser = tree_sitter::Parser::new();
        parser.set_language(&lang.tree_sitter_language()).unwrap();
        let tree = parser.parse(source, None).unwrap();
        project(lang, &tree, source.as_bytes()).unwrap()
    }

    #[cfg(feature = "lang-wsdl")]
    fn callers_of(lang: Lang, source: &str, callee: &str) -> Vec<String> {
        let symbols = lang.extract_symbols(source.as_bytes()).unwrap();
        let sites = crate::extract_call_sites(lang, source.as_bytes()).unwrap();
        let mut callers: Vec<String> = crate::resolve_edges(&symbols, &sites)
            .into_iter()
            .filter(|edge| edge.callee == callee)
            .map(|edge| edge.caller)
            .collect();
        callers.sort_unstable();
        callers
    }

    #[cfg(feature = "lang-wsdl")]
    const STOCK_WSDL11: &str = r#"<?xml version="1.0"?>
<definitions name="StockQuote"
    targetNamespace="http://example.com/stockquote.wsdl"
    xmlns:tns="http://example.com/stockquote.wsdl"
    xmlns:xsd1="http://example.com/stockquote.xsd"
    xmlns:soap="http://schemas.xmlsoap.org/wsdl/soap/"
    xmlns="http://schemas.xmlsoap.org/wsdl/">
  <types>
    <schema targetNamespace="http://example.com/stockquote.xsd"
            xmlns="http://www.w3.org/2001/XMLSchema">
      <element name="TradePriceRequest">
        <complexType><all><element name="tickerSymbol" type="string"/></all></complexType>
      </element>
      <element name="TradePrice">
        <complexType><all><element name="price" type="float"/></all></complexType>
      </element>
    </schema>
  </types>
  <message name="GetLastTradePriceInput">
    <part name="body" element="xsd1:TradePriceRequest"/>
  </message>
  <message name="GetLastTradePriceOutput">
    <part name="body" element="xsd1:TradePrice"/>
  </message>
  <message name="GetTradeHistoryInput">
    <part name="body" element="xsd1:TradePriceRequest"/>
  </message>
  <portType name="StockQuotePortType">
    <operation name="GetLastTradePrice">
      <input message="tns:GetLastTradePriceInput"/>
      <output message="tns:GetLastTradePriceOutput"/>
    </operation>
    <operation name="GetTradeHistory">
      <input message="tns:GetTradeHistoryInput"/>
    </operation>
  </portType>
  <binding name="StockQuoteSoapBinding" type="tns:StockQuotePortType">
    <soap:binding style="document" transport="http://schemas.xmlsoap.org/soap/http"/>
    <operation name="GetLastTradePrice">
      <soap:operation soapAction="http://example.com/GetLastTradePrice"/>
    </operation>
  </binding>
  <service name="StockQuoteService">
    <documentation>My first service</documentation>
    <port name="StockQuotePort" binding="tns:StockQuoteSoapBinding">
      <soap:address location="http://example.com/stockquote"/>
    </port>
  </service>
</definitions>
"#;

    #[cfg(feature = "lang-wsdl")]
    #[test]
    fn wsdl11_projects_components_and_embedded_schema() {
        let projection = wsdl_projection(STOCK_WSDL11);
        assert_eq!(
            names(&projection.symbols),
            vec![
                pair("schema", "TradePriceRequest"),
                pair("property", "TradePriceRequest.tickerSymbol"),
                pair("schema", "TradePrice"),
                pair("property", "TradePrice.price"),
                pair("message", "GetLastTradePriceInput"),
                pair("property", "GetLastTradePriceInput.body"),
                pair("message", "GetLastTradePriceOutput"),
                pair("property", "GetLastTradePriceOutput.body"),
                pair("message", "GetTradeHistoryInput"),
                pair("property", "GetTradeHistoryInput.body"),
                pair("interface", "StockQuotePortType"),
                pair("operation", "StockQuotePortType.GetLastTradePrice"),
                pair("operation", "StockQuotePortType.GetTradeHistory"),
                pair("binding", "StockQuoteSoapBinding"),
                pair("operation", "StockQuoteSoapBinding.GetLastTradePrice"),
                pair("service", "StockQuoteService"),
                pair("endpoint", "StockQuoteService.StockQuotePort"),
            ]
        );
        let port_type = projection
            .symbols
            .iter()
            .find(|symbol| symbol.name == "StockQuotePortType")
            .unwrap();
        assert_eq!(port_type.node_kind, "portType");
        assert!(
            STOCK_WSDL11[port_type.start_byte..port_type.end_byte]
                .starts_with("<portType name=\"StockQuotePortType\">")
        );
    }

    #[cfg(feature = "lang-wsdl")]
    #[test]
    fn wsdl11_references_chain_service_to_schema() {
        let lang = Lang::Wsdl;
        // Embedded schema refs to built-ins (`string`, `float`) yield nothing.
        assert!(callers_of(lang, STOCK_WSDL11, "string").is_empty());
        assert_eq!(
            callers_of(lang, STOCK_WSDL11, "TradePriceRequest"),
            vec!["GetLastTradePriceInput", "GetTradeHistoryInput"]
        );
        assert_eq!(
            callers_of(lang, STOCK_WSDL11, "GetLastTradePriceInput"),
            vec!["StockQuotePortType.GetLastTradePrice"]
        );
        assert_eq!(
            callers_of(lang, STOCK_WSDL11, "StockQuotePortType"),
            vec!["StockQuoteSoapBinding"]
        );
        assert_eq!(
            callers_of(lang, STOCK_WSDL11, "StockQuoteSoapBinding"),
            vec!["StockQuoteService.StockQuotePort"]
        );
    }

    #[cfg(feature = "lang-wsdl")]
    const RESERVATION_WSDL20: &str = r##"<?xml version="1.0"?>
<wsdl:description xmlns:wsdl="http://www.w3.org/ns/wsdl"
    targetNamespace="http://example.com/reservation"
    xmlns:tns="http://example.com/reservation"
    xmlns:ghns="http://example.com/reservation/schema"
    xmlns:wsoap="http://www.w3.org/ns/wsdl/soap">
  <wsdl:types>
    <xs:schema xmlns:xs="http://www.w3.org/2001/XMLSchema"
               targetNamespace="http://example.com/reservation/schema">
      <xs:element name="checkAvailability" type="ghns:tCheckAvailability"/>
      <xs:complexType name="tCheckAvailability">
        <xs:sequence><xs:element name="checkInDate" type="xs:date"/></xs:sequence>
      </xs:complexType>
      <xs:element name="checkAvailabilityResponse" type="xs:double"/>
    </xs:schema>
  </wsdl:types>
  <wsdl:interface name="reservationInterface">
    <wsdl:operation name="opCheckAvailability" pattern="http://www.w3.org/ns/wsdl/in-out">
      <wsdl:input messageLabel="In" element="ghns:checkAvailability"/>
      <wsdl:output messageLabel="Out" element="ghns:checkAvailabilityResponse"/>
    </wsdl:operation>
    <wsdl:operation name="opQuoteAvailability" pattern="http://www.w3.org/ns/wsdl/in-out">
      <wsdl:input messageLabel="In" element="ghns:checkAvailability"/>
      <wsdl:output messageLabel="Out" element="#any"/>
    </wsdl:operation>
  </wsdl:interface>
  <wsdl:binding name="reservationSOAPBinding" interface="tns:reservationInterface"
      type="http://www.w3.org/ns/wsdl/soap" wsoap:protocol="http://www.w3.org/2003/05/soap/bindings/HTTP/">
    <wsdl:operation ref="tns:opCheckAvailability" wsoap:mep="http://www.w3.org/2003/05/soap/mep/soap-response"/>
  </wsdl:binding>
  <wsdl:service name="reservationService" interface="tns:reservationInterface">
    <wsdl:endpoint name="reservationEndpoint" binding="tns:reservationSOAPBinding"
        address="http://greath.example.com/2004/reservation"/>
  </wsdl:service>
</wsdl:description>
"##;

    #[cfg(feature = "lang-wsdl")]
    #[test]
    fn wsdl20_projects_interfaces_and_operation_element_refs() {
        let projection = wsdl_projection(RESERVATION_WSDL20);
        assert_eq!(
            names(&projection.symbols),
            vec![
                pair("schema", "checkAvailability"),
                pair("schema", "tCheckAvailability"),
                pair("property", "tCheckAvailability.checkInDate"),
                pair("schema", "checkAvailabilityResponse"),
                pair("interface", "reservationInterface"),
                pair("operation", "reservationInterface.opCheckAvailability"),
                pair("operation", "reservationInterface.opQuoteAvailability"),
                pair("binding", "reservationSOAPBinding"),
                pair("service", "reservationService"),
                pair("endpoint", "reservationService.reservationEndpoint"),
            ]
        );
        let lang = Lang::Wsdl;
        // A schema element is called from every operation whose message
        // references it.
        assert_eq!(
            callers_of(lang, RESERVATION_WSDL20, "checkAvailability"),
            vec![
                "reservationInterface.opCheckAvailability",
                "reservationInterface.opQuoteAvailability",
            ]
        );
        assert_eq!(
            callers_of(lang, RESERVATION_WSDL20, "tCheckAvailability"),
            vec!["checkAvailability"]
        );
        assert_eq!(
            callers_of(lang, RESERVATION_WSDL20, "reservationInterface"),
            vec!["reservationSOAPBinding", "reservationService"]
        );
        assert_eq!(
            callers_of(lang, RESERVATION_WSDL20, "reservationSOAPBinding"),
            vec!["reservationService.reservationEndpoint"]
        );
        // The binding's `type=` is a URI, and `#any` is a keyword: neither is
        // a QName, so neither invents a callee.
        let callees: Vec<&str> = projection
            .sites
            .iter()
            .map(|site| site.callee.as_str())
            .collect();
        assert!(
            !callees
                .iter()
                .any(|callee| callee.contains('/') || callee.contains('#'))
        );
    }

    #[cfg(feature = "lang-wsdl")]
    #[test]
    fn non_wsdl_root_projects_nothing() {
        let projection = wsdl_projection(
            r#"<definitions xmlns="urn:not-wsdl"><message name="m"/></definitions>"#,
        );
        assert!(projection.symbols.is_empty());
        assert!(projection.sites.is_empty());
    }
}
