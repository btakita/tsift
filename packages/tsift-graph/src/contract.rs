//! Structural projection of API contract documents (`#sdktsiftcontracts`).
//!
//! JSON and YAML have no declarations of their own, so a generic walk over
//! every key would bury a schema in noise. This module instead recognizes the
//! three contract dialects -- JSON Schema, OpenAPI (and Swagger 2), AsyncAPI --
//! and projects only the nodes a reader navigates by: schema definitions and
//! their properties, path operations, channels, messages, and reusable
//! components. Any other JSON/YAML document (a `package.json`, a CI workflow)
//! projects to nothing; it is still indexed for full-text search.
//!
//! `$ref` pointers become call sites, so a component referenced from an
//! operation gets the same caller edges a function gets from its call sites.
//!
//! Both grammars are lowered into one small value tree first, so the projection
//! is written once and reads identically for a `.json` and a `.yaml` contract.

use crate::{CallSite, Symbol};

/// One parsed value, carrying the span of the syntax that produced it.
#[derive(Debug)]
pub(crate) struct Value {
    data: Data,
    start_byte: usize,
    end_byte: usize,
    line: usize,
    end_line: usize,
}

#[derive(Debug)]
enum Data {
    Map(Vec<Entry>),
    Seq(Vec<Value>),
    Scalar(String),
}

/// A key/value pair; its span covers both, the way a declaration covers its
/// name and body.
#[derive(Debug)]
struct Entry {
    key: String,
    value: Value,
    start_byte: usize,
    end_byte: usize,
    line: usize,
    end_line: usize,
    node_kind: &'static str,
}

impl Value {
    fn from_node(node: tree_sitter::Node<'_>, data: Data) -> Self {
        Self {
            data,
            start_byte: node.start_byte(),
            end_byte: node.end_byte(),
            line: node.start_position().row,
            end_line: node.end_position().row,
        }
    }

    fn get(&self, key: &str) -> Option<&Value> {
        self.entries()?
            .iter()
            .find(|entry| entry.key == key)
            .map(|entry| &entry.value)
    }

    fn entries(&self) -> Option<&[Entry]> {
        match &self.data {
            Data::Map(entries) => Some(entries),
            _ => None,
        }
    }

    fn scalar(&self) -> Option<&str> {
        match &self.data {
            Data::Scalar(text) => Some(text),
            _ => None,
        }
    }

    fn has(&self, key: &str) -> bool {
        self.get(key).is_some()
    }
}

fn node_text<'a>(node: tree_sitter::Node<'_>, source: &'a [u8]) -> &'a str {
    node.utf8_text(source).unwrap_or("")
}

/// Lower a tree-sitter-json tree.
pub(crate) fn lower_json(root: tree_sitter::Node<'_>, source: &[u8]) -> Option<Value> {
    let mut cursor = root.walk();
    let value = root
        .named_children(&mut cursor)
        .find(|child| child.kind() != "comment")?;
    Some(lower_json_value(value, source, 0))
}

fn lower_json_value(node: tree_sitter::Node<'_>, source: &[u8], depth: usize) -> Value {
    if depth > MAX_DEPTH {
        return Value::from_node(node, Data::Scalar(String::new()));
    }
    let mut cursor = node.walk();
    let data = match node.kind() {
        "object" => Data::Map(
            node.named_children(&mut cursor)
                .filter(|child| child.kind() == "pair")
                .filter_map(|pair| {
                    let key = pair.child_by_field_name("key")?;
                    let value = pair.child_by_field_name("value")?;
                    Some(Entry {
                        key: json_string(key, source),
                        value: lower_json_value(value, source, depth + 1),
                        start_byte: pair.start_byte(),
                        end_byte: pair.end_byte(),
                        line: pair.start_position().row,
                        end_line: pair.end_position().row,
                        node_kind: "pair",
                    })
                })
                .collect(),
        ),
        "array" => Data::Seq(
            node.named_children(&mut cursor)
                .filter(|child| child.kind() != "comment")
                .map(|child| lower_json_value(child, source, depth + 1))
                .collect(),
        ),
        "string" => Data::Scalar(json_string(node, source)),
        _ => Data::Scalar(node_text(node, source).to_string()),
    };
    Value::from_node(node, data)
}

fn json_string(node: tree_sitter::Node<'_>, source: &[u8]) -> String {
    let raw = node_text(node, source);
    serde_json::from_str::<String>(raw).unwrap_or_else(|_| raw.trim_matches('"').to_string())
}

/// Lower the first document of a tree-sitter-yaml stream. A contract is one
/// document; later documents in a multi-document stream are not projected.
pub(crate) fn lower_yaml(root: tree_sitter::Node<'_>, source: &[u8]) -> Option<Value> {
    let mut cursor = root.walk();
    let document = root
        .named_children(&mut cursor)
        .find(|child| child.kind() == "document")?;
    let content = yaml_content(document)?;
    Some(lower_yaml_value(content, source, 0))
}

/// The content child of a wrapper node, skipping anchors, tags, and comments.
fn yaml_content(node: tree_sitter::Node<'_>) -> Option<tree_sitter::Node<'_>> {
    let mut cursor = node.walk();
    node.named_children(&mut cursor)
        .filter(|child| !matches!(child.kind(), "anchor" | "tag" | "comment"))
        .last()
}

fn lower_yaml_value(node: tree_sitter::Node<'_>, source: &[u8], depth: usize) -> Value {
    if depth > MAX_DEPTH {
        return Value::from_node(node, Data::Scalar(String::new()));
    }
    if matches!(node.kind(), "block_node" | "flow_node") {
        return match yaml_content(node) {
            Some(content) => lower_yaml_value(content, source, depth + 1),
            None => Value::from_node(node, Data::Scalar(String::new())),
        };
    }
    let mut cursor = node.walk();
    let data = match node.kind() {
        "block_mapping" | "flow_mapping" => Data::Map(
            node.named_children(&mut cursor)
                .filter(|child| matches!(child.kind(), "block_mapping_pair" | "flow_pair"))
                .filter_map(|pair| {
                    let key = pair.child_by_field_name("key")?;
                    let key = yaml_scalar(yaml_leaf(key), source);
                    let value = match pair.child_by_field_name("value") {
                        Some(value) => lower_yaml_value(value, source, depth + 1),
                        None => Value::from_node(pair, Data::Scalar(String::new())),
                    };
                    Some(Entry {
                        key,
                        value,
                        start_byte: pair.start_byte(),
                        end_byte: pair.end_byte(),
                        line: pair.start_position().row,
                        end_line: pair.end_position().row,
                        node_kind: if pair.kind() == "flow_pair" {
                            "flow_pair"
                        } else {
                            "block_mapping_pair"
                        },
                    })
                })
                .collect(),
        ),
        "block_sequence" => Data::Seq(
            node.named_children(&mut cursor)
                .filter(|child| child.kind() == "block_sequence_item")
                .filter_map(|item| yaml_content(item))
                .map(|child| lower_yaml_value(child, source, depth + 1))
                .collect(),
        ),
        "flow_sequence" => Data::Seq(
            node.named_children(&mut cursor)
                .filter(|child| child.kind() != "comment")
                .map(|child| lower_yaml_value(child, source, depth + 1))
                .collect(),
        ),
        _ => Data::Scalar(yaml_scalar(node, source)),
    };
    Value::from_node(node, data)
}

/// Descend wrapper nodes to the scalar that carries a key's text.
fn yaml_leaf(mut node: tree_sitter::Node<'_>) -> tree_sitter::Node<'_> {
    while matches!(node.kind(), "flow_node" | "block_node" | "plain_scalar") {
        match yaml_content(node) {
            Some(child) => node = child,
            None => break,
        }
    }
    node
}

fn yaml_scalar(node: tree_sitter::Node<'_>, source: &[u8]) -> String {
    let raw = node_text(node, source);
    match node.kind() {
        "double_quote_scalar" => serde_json::from_str::<String>(raw)
            .unwrap_or_else(|_| raw.trim_matches('"').to_string()),
        "single_quote_scalar" => raw
            .strip_prefix('\'')
            .and_then(|s| s.strip_suffix('\''))
            .unwrap_or(raw)
            .replace("''", "'"),
        _ => raw.trim().to_string(),
    }
}

/// Bound on nesting, in lowering and in projection. Real contracts nest a few
/// levels; a pathological document must not overflow the stack.
const MAX_DEPTH: usize = 64;

/// Which contract dialect a document is written in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Dialect {
    JsonSchema,
    OpenApi,
    AsyncApi,
}

/// Classify a document by its root keys. Detection is deliberately narrow: a
/// false positive turns an ordinary config file into a pile of "schema"
/// symbols, while a miss only leaves it to full-text search.
fn dialect(root: &Value) -> Option<Dialect> {
    root.entries()?;
    if root.has("openapi") || root.has("swagger") {
        return Some(Dialect::OpenApi);
    }
    if root.has("asyncapi") {
        return Some(Dialect::AsyncApi);
    }
    if root.has("$schema")
        || root.has("$defs")
        || root.has("definitions")
        || (root.has("$id") && (root.has("type") || root.has("properties")))
    {
        return Some(Dialect::JsonSchema);
    }
    None
}

const HTTP_METHODS: &[&str] = &[
    "get", "put", "post", "delete", "options", "head", "patch", "trace",
];

struct Projector {
    symbols: Vec<Symbol>,
}

impl Projector {
    fn emit(&mut self, entry: &Entry, name: String, kind: &str) {
        if name.is_empty() {
            return;
        }
        self.symbols.push(Symbol {
            name,
            kind: kind.to_string(),
            line: entry.line,
            end_line: entry.end_line,
            node_kind: entry.node_kind.to_string(),
            start_byte: entry.start_byte,
            end_byte: entry.end_byte,
            body_start_byte: Some(entry.value.start_byte),
            body_end_byte: Some(entry.value.end_byte),
        });
    }

    /// Each entry of a named-collection map (`$defs`, `components/schemas`)
    /// becomes a schema, and its properties become `Owner.property`.
    fn schema_collection(&mut self, collection: Option<&Value>, depth: usize) {
        let Some(entries) = collection.and_then(Value::entries) else {
            return;
        };
        for entry in entries {
            self.emit(entry, entry.key.clone(), "schema");
            self.schema(&entry.value, Some(&entry.key), depth + 1);
        }
    }

    /// Walk one schema object in source order, so output stays line-ordered.
    fn schema(&mut self, schema: &Value, owner: Option<&str>, depth: usize) {
        if depth > MAX_DEPTH {
            return;
        }
        let Some(entries) = schema.entries() else {
            return;
        };
        for entry in entries {
            match entry.key.as_str() {
                "properties" => {
                    let Some(properties) = entry.value.entries() else {
                        continue;
                    };
                    for property in properties {
                        let name = match owner {
                            Some(owner) => format!("{owner}.{}", property.key),
                            None => property.key.clone(),
                        };
                        self.emit(property, name.clone(), "property");
                        self.schema(&property.value, Some(&name), depth + 1);
                    }
                }
                "$defs" | "definitions" => {
                    self.schema_collection(Some(&entry.value), depth);
                }
                "items" | "additionalProperties" | "not" | "if" | "then" | "else" | "contains" => {
                    self.schema(&entry.value, owner, depth + 1)
                }
                "allOf" | "anyOf" | "oneOf" | "prefixItems" => {
                    if let Data::Seq(members) = &entry.value.data {
                        for member in members {
                            self.schema(member, owner, depth + 1);
                        }
                    }
                }
                _ => {}
            }
        }
    }

    /// `components` holds schemas, which get the schema walk, and every other
    /// reusable object (responses, parameters, messages, ...) keyed by name.
    fn components(&mut self, components: Option<&Value>) {
        let Some(groups) = components.and_then(Value::entries) else {
            return;
        };
        for group in groups {
            match group.key.as_str() {
                "schemas" => self.schema_collection(Some(&group.value), 0),
                "messages" => self.named(&group.value, "message"),
                "operations" => self.named(&group.value, "operation"),
                "channels" => self.named(&group.value, "channel"),
                _ => self.named(&group.value, "component"),
            }
        }
    }

    fn named(&mut self, collection: &Value, kind: &str) {
        for entry in collection.entries().unwrap_or(&[]) {
            self.emit(entry, entry.key.clone(), kind);
        }
    }

    /// OpenAPI `paths` / `webhooks`: the path item is a symbol named by its
    /// route, each method an operation named by `operationId`.
    fn paths(&mut self, paths: &Value) {
        for item in paths.entries().unwrap_or(&[]) {
            self.emit(item, item.key.clone(), "path");
            for method in item.value.entries().unwrap_or(&[]) {
                if !HTTP_METHODS.contains(&method.key.as_str()) {
                    continue;
                }
                let name = method
                    .value
                    .get("operationId")
                    .and_then(Value::scalar)
                    .map(str::to_string)
                    .unwrap_or_else(|| format!("{} {}", method.key.to_uppercase(), item.key));
                self.emit(method, name, "operation");
            }
        }
    }

    fn openapi(&mut self, root: &Value) {
        for entry in root.entries().unwrap_or(&[]) {
            match entry.key.as_str() {
                "paths" | "webhooks" => self.paths(&entry.value),
                "components" => self.components(Some(&entry.value)),
                // Swagger 2 keeps its schemas at the root.
                "definitions" => self.schema_collection(Some(&entry.value), 0),
                _ => {}
            }
        }
    }

    fn asyncapi(&mut self, root: &Value) {
        for entry in root.entries().unwrap_or(&[]) {
            match entry.key.as_str() {
                "channels" => {
                    for channel in entry.value.entries().unwrap_or(&[]) {
                        self.emit(channel, channel.key.clone(), "channel");
                        for part in channel.value.entries().unwrap_or(&[]) {
                            match part.key.as_str() {
                                // AsyncAPI 3: messages live on the channel.
                                "messages" => self.named(&part.value, "message"),
                                // AsyncAPI 2: operations live on the channel.
                                "publish" | "subscribe" => {
                                    let name = part
                                        .value
                                        .get("operationId")
                                        .and_then(Value::scalar)
                                        .map(str::to_string)
                                        .unwrap_or_else(|| format!("{} {}", part.key, channel.key));
                                    self.emit(part, name, "operation");
                                }
                                _ => {}
                            }
                        }
                    }
                }
                "operations" => self.named(&entry.value, "operation"),
                "components" => self.components(Some(&entry.value)),
                _ => {}
            }
        }
    }
}

/// Project a lowered document into navigable symbols, line-ordered.
pub(crate) fn project(root: &Value) -> Vec<Symbol> {
    let mut projector = Projector {
        symbols: Vec::new(),
    };
    match dialect(root) {
        Some(Dialect::OpenApi) => projector.openapi(root),
        Some(Dialect::AsyncApi) => projector.asyncapi(root),
        Some(Dialect::JsonSchema) => {
            // A root schema is named by its `title`; untitled, its properties
            // are reported bare.
            let title = root
                .get("title")
                .and_then(Value::scalar)
                .map(str::to_string);
            if let Some(title) = &title {
                projector.symbols.push(Symbol {
                    name: title.clone(),
                    kind: "schema".to_string(),
                    line: root.line,
                    end_line: root.end_line,
                    node_kind: "document".to_string(),
                    start_byte: root.start_byte,
                    end_byte: root.end_byte,
                    body_start_byte: Some(root.start_byte),
                    body_end_byte: Some(root.end_byte),
                });
            }
            projector.schema(root, title.as_deref(), 0);
        }
        None => {}
    }
    let mut symbols = projector.symbols;
    symbols.sort_by_key(|symbol| (symbol.line, symbol.start_byte));
    symbols
}

/// Every `$ref` in a contract document, as a call site naming the last
/// segment of its JSON pointer (`#/components/schemas/Foo` -> `Foo`). A ref to
/// a whole external file has no in-document name and yields no site.
pub(crate) fn ref_sites(root: &Value) -> Vec<CallSite> {
    let mut sites = Vec::new();
    if dialect(root).is_some() {
        collect_refs(root, &mut sites, 0);
    }
    sites
}

fn collect_refs(value: &Value, sites: &mut Vec<CallSite>, depth: usize) {
    if depth > MAX_DEPTH {
        return;
    }
    match &value.data {
        Data::Map(entries) => {
            for entry in entries {
                if entry.key == "$ref"
                    && let Some(target) = entry.value.scalar().and_then(ref_target)
                {
                    sites.push(CallSite {
                        callee: target,
                        line: entry.line,
                    });
                }
                collect_refs(&entry.value, sites, depth + 1);
            }
        }
        Data::Seq(items) => {
            for item in items {
                collect_refs(item, sites, depth + 1);
            }
        }
        Data::Scalar(_) => {}
    }
}

fn ref_target(reference: &str) -> Option<String> {
    let (_, pointer) = reference.split_once('#')?;
    let segment = pointer.rsplit('/').next()?;
    if segment.is_empty() {
        return None;
    }
    Some(segment.replace("~1", "/").replace("~0", "~"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(lang: tree_sitter::Language, source: &str) -> tree_sitter::Tree {
        let mut parser = tree_sitter::Parser::new();
        parser.set_language(&lang).unwrap();
        parser.parse(source, None).unwrap()
    }

    #[cfg(feature = "lang-json")]
    fn json(source: &str) -> (Vec<Symbol>, Vec<CallSite>) {
        let tree = parse(tree_sitter_json::LANGUAGE.into(), source);
        let root = lower_json(tree.root_node(), source.as_bytes()).unwrap();
        (project(&root), ref_sites(&root))
    }

    #[cfg(feature = "lang-yaml")]
    fn yaml(source: &str) -> (Vec<Symbol>, Vec<CallSite>) {
        let tree = parse(tree_sitter_yaml::LANGUAGE.into(), source);
        let root = lower_yaml(tree.root_node(), source.as_bytes()).unwrap();
        (project(&root), ref_sites(&root))
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

    #[cfg(feature = "lang-json")]
    #[test]
    fn json_schema_projects_title_properties_and_defs() {
        let source = r##"{
  "$schema": "https://json-schema.org/draft/2020-12/schema",
  "title": "ChatMessageV0",
  "type": "object",
  "properties": {
    "id": {"type": "string"},
    "author": {"$ref": "#/$defs/Author"}
  },
  "$defs": {
    "Author": {"properties": {"name": {"type": "string"}}}
  }
}"##;
        let (symbols, sites) = json(source);
        assert_eq!(
            names(&symbols),
            vec![
                pair("schema", "ChatMessageV0"),
                pair("property", "ChatMessageV0.id"),
                pair("property", "ChatMessageV0.author"),
                pair("schema", "Author"),
                pair("property", "Author.name"),
            ]
        );
        let author = &symbols[3];
        assert!(source[author.start_byte..author.end_byte].starts_with("\"Author\""));
        assert_eq!(sites.len(), 1);
        assert_eq!(sites[0].callee, "Author");
        assert_eq!(sites[0].line, 6);
    }

    #[cfg(feature = "lang-json")]
    #[test]
    fn openapi_projects_paths_operations_and_components() {
        let source = r##"{
  "openapi": "3.1.0",
  "paths": {
    "/v1/chat/{id}": {
      "parameters": [],
      "get": {"operationId": "getChat",
        "responses": {"200": {"content": {"application/json": {"schema": {"$ref": "#/components/schemas/Chat"}}}}}},
      "delete": {}
    }
  },
  "components": {
    "schemas": {"Chat": {"type": "object", "properties": {"id": {"type": "string"}}}},
    "parameters": {"ChatId": {"in": "path"}}
  }
}"##;
        let (symbols, sites) = json(source);
        assert_eq!(
            names(&symbols),
            vec![
                pair("path", "/v1/chat/{id}"),
                pair("operation", "getChat"),
                pair("operation", "DELETE /v1/chat/{id}"),
                pair("schema", "Chat"),
                pair("property", "Chat.id"),
                pair("component", "ChatId"),
            ]
        );
        assert_eq!(sites.len(), 1);
        assert_eq!(sites[0].callee, "Chat");
    }

    #[cfg(feature = "lang-json")]
    #[test]
    fn asyncapi_v3_projects_channels_messages_operations() {
        let source = r##"{
  "asyncapi": "3.0.0",
  "channels": {
    "decisionV0": {
      "address": "tenant.{tenantId}.decision",
      "messages": {"decisionV0": {"payload": {"$ref": "#/components/schemas/decisionV0"}}}
    }
  },
  "operations": {"sendDecision": {"action": "send"}},
  "components": {"schemas": {"decisionV0": {"type": "object"}}}
}"##;
        let (symbols, sites) = json(source);
        assert_eq!(
            names(&symbols),
            vec![
                pair("channel", "decisionV0"),
                pair("message", "decisionV0"),
                pair("operation", "sendDecision"),
                pair("schema", "decisionV0"),
            ]
        );
        assert_eq!(sites[0].callee, "decisionV0");
    }

    #[cfg(feature = "lang-json")]
    #[test]
    fn ordinary_json_projects_nothing() {
        let (symbols, sites) =
            json(r##"{"name": "pkg", "properties": {"x": 1}, "$ref": "#/a/b"}"##);
        assert!(symbols.is_empty());
        assert!(sites.is_empty());
    }

    #[cfg(feature = "lang-yaml")]
    #[test]
    fn yaml_openapi_matches_json_projection() {
        let source = "\
openapi: 3.0.3
paths:
  /health:
    get:
      operationId: getHealth
      responses:
        '200':
          content:
            application/json:
              schema:
                $ref: '#/components/schemas/Health'
components:
  schemas:
    Health:
      type: object
      properties:
        status: {type: string}
";
        let (symbols, sites) = yaml(source);
        assert_eq!(
            names(&symbols),
            vec![
                pair("path", "/health"),
                pair("operation", "getHealth"),
                pair("schema", "Health"),
                pair("property", "Health.status"),
            ]
        );
        let health = &symbols[2];
        assert!(source[health.start_byte..health.end_byte].starts_with("Health:"));
        assert_eq!(health.line, 13);
        assert_eq!(sites.len(), 1);
        assert_eq!(sites[0].callee, "Health");
        assert_eq!(sites[0].line, 10);
    }

    #[cfg(feature = "lang-yaml")]
    #[test]
    fn yaml_config_projects_nothing() {
        let (symbols, _) = yaml("name: CI\non:\n  push: {}\njobs:\n  test: {}\n");
        assert!(symbols.is_empty());
    }

    /// Through the public entry points the indexer uses: a `$ref` inside an
    /// operation is an edge from that operation, and one inside a property
    /// is attributed to the schema that declares the property.
    #[cfg(feature = "lang-json")]
    #[test]
    fn refs_resolve_to_edges_from_the_enclosing_contract_node() {
        let source = br##"{
  "openapi": "3.1.0",
  "paths": {"/a": {"get": {"operationId": "getA",
    "responses": {"200": {"$ref": "#/components/responses/AOk"}}}}},
  "components": {
    "responses": {"AOk": {"description": "ok"}},
    "schemas": {"A": {"properties": {"b": {"$ref": "#/components/schemas/B"}}}, "B": {}}
  }
}"##;
        let lang = crate::Lang::Json;
        let symbols = lang.extract_symbols(source).unwrap();
        let sites = crate::extract_call_sites(lang, source).unwrap();
        let edges: Vec<(String, String)> = crate::resolve_edges(&symbols, &sites)
            .into_iter()
            .map(|edge| (edge.caller, edge.callee))
            .collect();
        assert_eq!(
            edges,
            vec![
                ("getA".to_string(), "AOk".to_string()),
                ("A".to_string(), "B".to_string()),
            ]
        );
    }

    #[test]
    fn ref_target_takes_the_last_pointer_segment() {
        assert_eq!(ref_target("#/$defs/Foo").as_deref(), Some("Foo"));
        assert_eq!(
            ref_target("common.json#/components/schemas/Bar").as_deref(),
            Some("Bar")
        );
        assert_eq!(ref_target("#/paths/~1v1~1x").as_deref(), Some("/v1/x"));
        assert_eq!(ref_target("./other.json"), None);
        assert_eq!(ref_target("#"), None);
    }
}
