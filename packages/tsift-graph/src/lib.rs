use anyhow::Result;
use lazily::{CellHandle, Context as LazyContext, SlotHandle};
use serde::{Deserialize, Serialize};
use std::cell::{Cell, RefCell};
use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};
use tree_sitter::{Parser, Query, QueryCursor, StreamingIterator};
use tsift_core::{GraphEdge, GraphNode, GraphProjection, GraphProvenance};

pub mod lang;
pub use lang::{Lang, Symbol};

pub mod complexity;
pub use complexity::{ComplexityMetrics, LanguageExtractor, LanguageRegistry};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CallSite {
    pub callee: String,
    pub line: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CallEdge {
    pub caller: String,
    pub callee: String,
    pub caller_line: usize,
    pub call_site_line: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FileMtime {
    pub secs: i64,
    pub nanos: u32,
}

impl FileMtime {
    pub fn new(secs: i64, nanos: u32) -> Self {
        Self { secs, nanos }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct ResolveEdgesKey {
    file: PathBuf,
    content_hash: String,
}

#[derive(Clone, Copy)]
struct ResolveEdgesSlot {
    mtime: CellHandle<FileMtime>,
    edges: SlotHandle<Vec<CallEdge>>,
}

pub struct ResolveEdgesCache {
    ctx: LazyContext,
    slots: RefCell<HashMap<ResolveEdgesKey, ResolveEdgesSlot>>,
    hits: Cell<usize>,
    misses: Cell<usize>,
}

impl Default for ResolveEdgesCache {
    fn default() -> Self {
        Self::new()
    }
}

impl ResolveEdgesCache {
    pub fn new() -> Self {
        Self {
            ctx: LazyContext::new(),
            slots: RefCell::new(HashMap::new()),
            hits: Cell::new(0),
            misses: Cell::new(0),
        }
    }

    pub fn resolve_edges_for_file(
        &self,
        file: &Path,
        content_hash: &str,
        mtime: FileMtime,
        symbols: &[Symbol],
        call_sites: &[CallSite],
    ) -> Vec<CallEdge> {
        let key = ResolveEdgesKey {
            file: file.to_path_buf(),
            content_hash: content_hash.to_string(),
        };
        let slot = {
            let mut slots = self.slots.borrow_mut();
            if let Some(slot) = slots.get(&key) {
                self.ctx.set_cell(&slot.mtime, mtime);
                *slot
            } else {
                let mtime_cell = self.ctx.cell(mtime);
                let symbols = symbols.to_vec();
                let call_sites = call_sites.to_vec();
                let edges = self.ctx.slot(move |ctx| {
                    let _mtime = ctx.get_cell(&mtime_cell);
                    resolve_edges_uncached(&symbols, &call_sites)
                });
                let slot = ResolveEdgesSlot {
                    mtime: mtime_cell,
                    edges,
                };
                slots.insert(key, slot);
                slot
            }
        };
        if self.ctx.is_set(&slot.edges) {
            self.hits.set(self.hits.get() + 1);
        } else {
            self.misses.set(self.misses.get() + 1);
        }
        self.ctx.get(&slot.edges)
    }

    pub fn stats(&self) -> (usize, usize) {
        (self.hits.get(), self.misses.get())
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RouteSite {
    pub framework: String,
    pub method: Option<String>,
    pub path: String,
    pub handler: String,
    pub line: usize,
    pub handler_line: Option<usize>,
}

#[derive(Debug, Clone)]
struct PendingRoute {
    framework: String,
    method: Option<String>,
    path: String,
    line: usize,
}

pub fn extract_call_sites(lang: Lang, source: &[u8]) -> Result<Vec<CallSite>> {
    let query_str = match lang.call_query() {
        Some(q) => q,
        None => return Ok(Vec::new()),
    };
    let mut parser = Parser::new();
    let ts_lang = lang.tree_sitter_language();
    parser.set_language(&ts_lang)?;
    let tree = parser
        .parse(source, None)
        .ok_or_else(|| anyhow::anyhow!("parse failed"))?;
    let query = Query::new(&ts_lang, query_str)?;
    let mut cursor = QueryCursor::new();
    let mut sites = Vec::new();
    let capture_names: Vec<String> = query
        .capture_names()
        .iter()
        .map(|s| s.to_string())
        .collect();

    let mut matches = cursor.matches(&query, tree.root_node(), source);
    while let Some(m) = matches.next() {
        for capture in m.captures {
            let name = &capture_names[capture.index as usize];
            if name == "call.name" {
                let callee = capture
                    .node
                    .utf8_text(source)
                    .unwrap_or("<invalid utf8>")
                    .to_string();
                sites.push(CallSite {
                    callee,
                    line: capture.node.start_position().row,
                });
            }
        }
    }
    Ok(sites)
}

pub fn source_content_hash(source: &[u8]) -> String {
    blake3::hash(source).to_hex().to_string()
}

pub fn extract_route_sites(lang: Lang, source: &[u8]) -> Result<Vec<RouteSite>> {
    let text = std::str::from_utf8(source)?;
    Ok(match lang {
        #[cfg(feature = "lang-rust")]
        Lang::Rust => extract_rust_routes(text),
        #[cfg(feature = "lang-python")]
        Lang::Python => extract_python_routes(text),
        #[cfg(feature = "lang-typescript")]
        Lang::TypeScript | Lang::Tsx => extract_typescript_routes(text),
        #[cfg(feature = "lang-javascript")]
        Lang::JavaScript | Lang::Jsx => extract_typescript_routes(text),
        _ => Vec::new(),
    })
}

fn extract_string_literal(input: &str) -> Option<(String, usize)> {
    let mut chars = input.char_indices();
    while let Some((start, ch)) = chars.next() {
        if ch != '"' && ch != '\'' {
            continue;
        }
        let quote = ch;
        let mut escaped = false;
        let mut value = String::new();
        for (offset, current) in chars.by_ref() {
            if escaped {
                value.push(current);
                escaped = false;
                continue;
            }
            if current == '\\' {
                escaped = true;
                continue;
            }
            if current == quote {
                return Some((value, offset + current.len_utf8()));
            }
            value.push(current);
        }
        return Some((input[start + quote.len_utf8()..].to_string(), input.len()));
    }
    None
}

fn first_identifier(input: &str) -> Option<String> {
    let mut start = None;
    for (idx, ch) in input.char_indices() {
        if start.is_none() {
            if ch == '_' || ch.is_ascii_alphabetic() {
                start = Some(idx);
            }
            continue;
        }
        if !(ch == '_' || ch.is_ascii_alphanumeric()) {
            let value = input[start.unwrap()..idx].to_string();
            return (!is_handler_keyword(&value)).then_some(value);
        }
    }
    start
        .map(|idx| input[idx..].to_string())
        .filter(|value| !is_handler_keyword(value))
}

fn is_handler_keyword(value: &str) -> bool {
    matches!(
        value,
        "async" | "await" | "function" | "move" | "None" | "Some" | "lambda"
    )
}

fn route_methods() -> &'static [&'static str] {
    &[
        "get", "post", "put", "patch", "delete", "head", "options", "any", "route",
    ]
}

fn parse_wrapped_handler(input: &str) -> (Option<String>, Option<String>) {
    for method in route_methods() {
        let needle = format!("{method}(");
        if let Some(pos) = input.find(&needle) {
            let inside = &input[pos + needle.len()..];
            return (
                Some((*method).to_string()),
                first_identifier(inside).or_else(|| Some("<inline>".to_string())),
            );
        }
    }
    (None, first_identifier(input))
}

fn parse_rust_fn_name(line: &str) -> Option<String> {
    let pos = line.find("fn ")?;
    first_identifier(&line[pos + 3..])
}

fn parse_route_attribute(line: &str, framework: &str) -> Option<PendingRoute> {
    let trimmed = line.trim_start();
    let rest = trimmed.strip_prefix("#[")?;
    for method in route_methods() {
        let Some(method_rest) = rest.strip_prefix(method) else {
            continue;
        };
        if !method_rest.trim_start().starts_with('(') {
            continue;
        }
        let (path, _) = extract_string_literal(method_rest)?;
        return Some(PendingRoute {
            framework: framework.to_string(),
            method: Some((*method).to_string()),
            path,
            line: 0,
        });
    }
    None
}

fn extract_rust_routes(text: &str) -> Vec<RouteSite> {
    let mut routes = Vec::new();
    let mut pending = Vec::<PendingRoute>::new();

    for (line_idx, line) in text.lines().enumerate() {
        if let Some(mut attr) = parse_route_attribute(line, "actix") {
            attr.line = line_idx;
            if let Some(handler) = parse_rust_fn_name(line) {
                routes.push(RouteSite {
                    framework: attr.framework,
                    method: attr.method,
                    path: attr.path,
                    handler,
                    line: attr.line,
                    handler_line: Some(line_idx),
                });
            } else {
                pending.push(attr);
            }
        } else if !pending.is_empty()
            && let Some(handler) = parse_rust_fn_name(line)
        {
            for attr in pending.drain(..) {
                routes.push(RouteSite {
                    framework: attr.framework,
                    method: attr.method,
                    path: attr.path,
                    handler: handler.clone(),
                    line: attr.line,
                    handler_line: Some(line_idx),
                });
            }
        }

        if let Some(route_pos) = line.find(".route(") {
            let route_args = &line[route_pos + ".route(".len()..];
            if let Some((path, end_offset)) = extract_string_literal(route_args) {
                let args_after_path = &route_args[end_offset..];
                let (method, handler) = parse_wrapped_handler(args_after_path);
                if let Some(handler) = handler {
                    routes.push(RouteSite {
                        framework: "axum".to_string(),
                        method: method.or_else(|| Some("route".to_string())),
                        path,
                        handler,
                        line: line_idx,
                        handler_line: None,
                    });
                }
            }
        }
    }

    routes
}

fn parse_python_def_name(line: &str) -> Option<String> {
    let trimmed = line.trim_start();
    let rest = trimmed
        .strip_prefix("async def ")
        .or_else(|| trimmed.strip_prefix("def "))?;
    first_identifier(rest)
}

fn parse_python_route_decorator(line: &str) -> Option<PendingRoute> {
    let trimmed = line.trim_start();
    let rest = trimmed.strip_prefix('@')?;
    let dot = rest.find('.')?;
    let after_dot = &rest[dot + 1..];
    for method in route_methods() {
        let Some(method_rest) = after_dot.strip_prefix(method) else {
            continue;
        };
        if !method_rest.trim_start().starts_with('(') {
            continue;
        }
        let (path, _) = extract_string_literal(method_rest)?;
        let framework = if *method == "route" {
            "flask"
        } else {
            "fastapi"
        };
        return Some(PendingRoute {
            framework: framework.to_string(),
            method: Some((*method).to_string()),
            path,
            line: 0,
        });
    }
    None
}

fn extract_python_routes(text: &str) -> Vec<RouteSite> {
    let mut routes = Vec::new();
    let mut pending = Vec::<PendingRoute>::new();

    for (line_idx, line) in text.lines().enumerate() {
        if let Some(mut route) = parse_python_route_decorator(line) {
            route.line = line_idx;
            pending.push(route);
            continue;
        }

        if !pending.is_empty()
            && let Some(handler) = parse_python_def_name(line)
        {
            for route in pending.drain(..) {
                routes.push(RouteSite {
                    framework: route.framework,
                    method: route.method,
                    path: route.path,
                    handler: handler.clone(),
                    line: route.line,
                    handler_line: Some(line_idx),
                });
            }
        }
    }

    routes
}

fn parse_ts_method_name(line: &str) -> Option<String> {
    let trimmed = line.trim_start();
    first_identifier(trimmed)
}

fn parse_ts_route_decorator(line: &str) -> Option<PendingRoute> {
    let trimmed = line.trim_start();
    let rest = trimmed.strip_prefix('@')?;
    for method in route_methods() {
        let mut chars = method.chars();
        let title = match chars.next() {
            Some(first) => format!("{}{}", first.to_ascii_uppercase(), chars.as_str()),
            None => continue,
        };
        let Some(method_rest) = rest.strip_prefix(&title) else {
            continue;
        };
        if !method_rest.trim_start().starts_with('(') {
            continue;
        }
        let (path, _) = extract_string_literal(method_rest)?;
        return Some(PendingRoute {
            framework: "nestjs".to_string(),
            method: Some((*method).to_string()),
            path,
            line: 0,
        });
    }
    None
}

fn parse_ts_router_call(line: &str, line_idx: usize) -> Option<RouteSite> {
    for method in route_methods() {
        if *method == "route" {
            continue;
        }
        let needle = format!(".{method}(");
        let Some(pos) = line.find(&needle) else {
            continue;
        };
        let args = &line[pos + needle.len()..];
        let (path, end_offset) = extract_string_literal(args)?;
        let handler = args[end_offset..]
            .split_once(',')
            .and_then(|(_, rest)| first_identifier(rest))
            .unwrap_or_else(|| "<inline>".to_string());
        return Some(RouteSite {
            framework: "express".to_string(),
            method: Some((*method).to_string()),
            path,
            handler,
            line: line_idx,
            handler_line: None,
        });
    }
    None
}

fn extract_typescript_routes(text: &str) -> Vec<RouteSite> {
    let mut routes = Vec::new();
    let mut pending = Vec::<PendingRoute>::new();

    for (line_idx, line) in text.lines().enumerate() {
        if let Some(mut route) = parse_ts_route_decorator(line) {
            route.line = line_idx;
            pending.push(route);
            continue;
        }

        if !pending.is_empty()
            && let Some(handler) = parse_ts_method_name(line)
        {
            for route in pending.drain(..) {
                routes.push(RouteSite {
                    framework: route.framework,
                    method: route.method,
                    path: route.path,
                    handler: handler.clone(),
                    line: route.line,
                    handler_line: Some(line_idx),
                });
            }
        }

        if let Some(route) = parse_ts_router_call(line, line_idx) {
            routes.push(route);
        }
    }

    routes
}

pub fn resolve_edges(symbols: &[Symbol], call_sites: &[CallSite]) -> Vec<CallEdge> {
    resolve_edges_uncached(symbols, call_sites)
}

fn resolve_edges_uncached(symbols: &[Symbol], call_sites: &[CallSite]) -> Vec<CallEdge> {
    let mut edges = Vec::new();
    for site in call_sites {
        let caller = symbols
            .iter()
            .filter(|s| s.kind == "function" || s.kind == "class" || s.kind == "mod")
            .filter(|s| site.line >= s.line && site.line <= s.end_line)
            .min_by_key(|s| s.end_line - s.line);
        if let Some(caller) = caller {
            edges.push(CallEdge {
                caller: caller.name.clone(),
                callee: site.callee.clone(),
                caller_line: caller.line,
                call_site_line: site.line,
            });
        }
    }
    edges
}

pub fn code_symbol_node_id(name: &str) -> String {
    format!("code.symbol:{name}")
}

pub fn code_route_node_id(framework: &str, method: Option<&str>, path: &str) -> String {
    format!(
        "code.route:{}:{}:{}",
        framework,
        method.unwrap_or("any"),
        path
    )
}

pub fn project_call_edges(
    edges: &[CallEdge],
    provenance: Option<GraphProvenance>,
) -> GraphProjection {
    let mut nodes = BTreeMap::<String, GraphNode>::new();
    let mut projected_edges = Vec::with_capacity(edges.len());

    for edge in edges {
        let caller_id = code_symbol_node_id(&edge.caller);
        let callee_id = code_symbol_node_id(&edge.callee);
        for (id, label) in [(&caller_id, &edge.caller), (&callee_id, &edge.callee)] {
            nodes.entry(id.clone()).or_insert_with(|| {
                let mut node = GraphNode::new(id.clone(), "code_symbol", label.clone());
                if let Some(provenance) = provenance.clone() {
                    node = node.with_provenance(provenance);
                }
                node
            });
        }

        let mut projected = GraphEdge::new(caller_id, callee_id, "calls")
            .with_property("caller_line", edge.caller_line.to_string())
            .with_property("call_site_line", edge.call_site_line.to_string());
        if let Some(provenance) = provenance.clone() {
            projected = projected.with_provenance(provenance);
        }
        projected_edges.push(projected);
    }

    GraphProjection {
        nodes: nodes.into_values().collect(),
        edges: projected_edges,
    }
}

pub fn project_routes(
    routes: &[RouteSite],
    provenance: Option<GraphProvenance>,
) -> GraphProjection {
    let mut nodes = BTreeMap::<String, GraphNode>::new();
    let mut projected_edges = Vec::with_capacity(routes.len());

    for route in routes {
        let route_id = code_route_node_id(&route.framework, route.method.as_deref(), &route.path);
        let handler_id = code_symbol_node_id(&route.handler);
        let mut route_node = GraphNode::new(
            route_id.clone(),
            "route",
            format!(
                "{} {}",
                route.method.as_deref().unwrap_or("any").to_uppercase(),
                route.path
            ),
        )
        .with_property("framework", route.framework.clone())
        .with_property("path", route.path.clone())
        .with_property("handler", route.handler.clone())
        .with_property("line", route.line.to_string());
        if let Some(method) = &route.method {
            route_node = route_node.with_property("method", method.clone());
        }
        if let Some(provenance) = provenance.clone() {
            route_node = route_node.with_provenance(provenance);
        }
        nodes.entry(route_id.clone()).or_insert(route_node);

        nodes.entry(handler_id.clone()).or_insert_with(|| {
            let mut node = GraphNode::new(handler_id.clone(), "code_symbol", route.handler.clone());
            if let Some(provenance) = provenance.clone() {
                node = node.with_provenance(provenance);
            }
            node
        });

        let mut edge = GraphEdge::new(route_id, handler_id, "handled_by")
            .with_property("route_path", route.path.clone())
            .with_property("framework", route.framework.clone());
        if let Some(method) = &route.method {
            edge = edge.with_property("method", method.clone());
        }
        if let Some(provenance) = provenance.clone() {
            edge = edge.with_provenance(provenance);
        }
        projected_edges.push(edge);
    }

    GraphProjection {
        nodes: nodes.into_values().collect(),
        edges: projected_edges,
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CommunityMemberRef {
    pub file: String,
    pub line: i64,
    pub role: String,
    pub peer: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CommunityMember {
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub file: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub line: Option<i64>,
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub refs: Vec<CommunityMemberRef>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub tagpath_handle: Option<String>,
}

impl CommunityMember {
    pub fn new(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            file: None,
            line: None,
            refs: Vec::new(),
            tagpath_handle: None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TerseCommunityMember {
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub tagpath_handle: Option<String>,
}

impl From<&CommunityMember> for TerseCommunityMember {
    fn from(m: &CommunityMember) -> Self {
        Self {
            name: m.name.clone(),
            tagpath_handle: m.tagpath_handle.clone(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TerseCommunity {
    pub id: usize,
    pub members: Vec<TerseCommunityMember>,
    pub modularity_contribution: f64,
}

impl TerseCommunity {
    pub fn from_community(community: &Community, top_n: usize) -> Self {
        let members: Vec<TerseCommunityMember> = community
            .members
            .iter()
            .take(top_n)
            .map(TerseCommunityMember::from)
            .collect();
        Self {
            id: community.id,
            members,
            modularity_contribution: community.modularity_contribution,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Community {
    pub id: usize,
    pub members: Vec<CommunityMember>,
    pub modularity_contribution: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CommunityResult {
    pub communities: Vec<Community>,
    pub modularity: f64,
    pub iterations: usize,
    pub node_count: usize,
    pub edge_count: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TerseCommunityResult {
    pub communities: Vec<TerseCommunity>,
    pub modularity: f64,
    pub iterations: usize,
    pub node_count: usize,
    pub edge_count: usize,
}

impl CommunityResult {
    pub fn to_terse(&self, top_n: usize) -> TerseCommunityResult {
        TerseCommunityResult {
            communities: self
                .communities
                .iter()
                .map(|c| TerseCommunity::from_community(c, top_n))
                .collect(),
            modularity: self.modularity,
            iterations: self.iterations,
            node_count: self.node_count,
            edge_count: self.edge_count,
        }
    }
}

struct LouvainGraph {
    n: usize,
    adj: Vec<HashMap<usize, f64>>,
    degree: Vec<f64>,
    m: f64,
}

impl LouvainGraph {
    fn from_indexed(n: usize, adj: Vec<HashSet<usize>>) -> Self {
        let degree: Vec<f64> = adj.iter().map(|nb| nb.len() as f64).collect();
        let m = degree.iter().sum::<f64>() / 2.0;
        let weighted: Vec<HashMap<usize, f64>> = adj
            .iter()
            .map(|nb| nb.iter().map(|&j| (j, 1.0_f64)).collect())
            .collect();
        Self {
            n,
            adj: weighted,
            degree,
            m,
        }
    }

    fn phase1(&self) -> (Vec<usize>, usize, bool) {
        let n = self.n;
        let m = self.m;
        let mut community: Vec<usize> = (0..n).collect();
        let mut comm_degree = self.degree.clone();
        let mut ki_in: Vec<HashMap<usize, f64>> = (0..n)
            .map(|i| {
                let mut map = HashMap::new();
                for (&nb, &w) in &self.adj[i] {
                    *map.entry(community[nb]).or_insert(0.0) += w;
                }
                map
            })
            .collect();

        let mut iterations = 0;
        let mut any_improved = false;
        loop {
            let mut improved = false;
            iterations += 1;

            for i in 0..n {
                let cur_c = community[i];
                let ki = self.degree[i];

                let ki_in_cur = ki_in[i].get(&cur_c).copied().unwrap_or(0.0);
                let cur_gain =
                    ki_in_cur / m - ki * (comm_degree[cur_c] - ki) / (2.0 * m * m);

                let mut best_delta = 0.0f64;
                let mut best_c = cur_c;

                for (&c, &ki_in_c) in &ki_in[i] {
                    if c == cur_c {
                        continue;
                    }
                    let target_gain = ki_in_c / m - ki * comm_degree[c] / (2.0 * m * m);
                    let delta = target_gain - cur_gain;
                    if delta > best_delta {
                        best_delta = delta;
                        best_c = c;
                    }
                }

                if best_c != cur_c {
                    comm_degree[cur_c] -= ki;
                    comm_degree[best_c] += ki;
                    for (&nb, &w) in &self.adj[i] {
                        ki_in[nb]
                            .entry(cur_c)
                            .and_modify(|v| *v -= w)
                            .or_insert(-w);
                        *ki_in[nb].entry(best_c).or_insert(0.0) += w;
                    }
                    community[i] = best_c;
                    improved = true;
                    any_improved = true;
                }
            }

            if !improved || iterations >= 100 {
                break;
            }
        }
        (community, iterations, any_improved)
    }

    fn coarsen(&self, community: &[usize]) -> LouvainGraph {
        let mut remap = HashMap::new();
        for &c in community {
            if !remap.contains_key(&c) {
                let idx = remap.len();
                remap.insert(c, idx);
            }
        }
        let n2 = remap.len();
        let mut adj2: Vec<HashMap<usize, f64>> = vec![HashMap::new(); n2];

        for i in 0..self.n {
            let ci = remap[&community[i]];
            for (&j, &w) in &self.adj[i] {
                let cj = remap[&community[j]];
                if ci == cj {
                    *adj2[ci].entry(ci).or_insert(0.0) += w / 2.0;
                } else {
                    *adj2[ci].entry(cj).or_insert(0.0) += w;
                }
            }
        }

        LouvainGraph::from_weighted(n2, adj2)
    }

    #[allow(dead_code)]
    fn from_weighted(n: usize, adj: Vec<HashMap<usize, f64>>) -> Self {
        let degree: Vec<f64> = (0..n).map(|i| adj[i].values().sum::<f64>()).collect();
        let m = degree.iter().sum::<f64>() / 2.0;
        Self { n, adj, degree, m }
    }
}

pub fn detect_communities(edges: &[(String, String)]) -> CommunityResult {
    if edges.is_empty() {
        return CommunityResult {
            communities: Vec::new(),
            modularity: 0.0,
            iterations: 0,
            node_count: 0,
            edge_count: 0,
        };
    }

    let mut node_vec: Vec<String> = Vec::new();
    let mut node_idx: HashMap<String, usize> = HashMap::new();
    for (a, b) in edges {
        for name in [a, b] {
            if !node_idx.contains_key(name) {
                node_idx.insert(name.clone(), node_vec.len());
                node_vec.push(name.clone());
            }
        }
    }
    let n = node_vec.len();

    let mut adj: Vec<HashSet<usize>> = vec![HashSet::new(); n];
    for (a, b) in edges {
        let ai = node_idx[a];
        let bi = node_idx[b];
        if ai != bi {
            adj[ai].insert(bi);
            adj[bi].insert(ai);
        }
    }

    let m = adj.iter().map(|nb| nb.len() as f64).sum::<f64>() / 2.0;

    if m == 0.0 {
        let communities = node_vec
            .iter()
            .enumerate()
            .map(|(i, name)| Community {
                id: i,
                members: vec![CommunityMember::new(name.clone())],
                modularity_contribution: 0.0,
            })
            .collect();
        return CommunityResult {
            communities,
            modularity: 0.0,
            iterations: 0,
            node_count: n,
            edge_count: 0,
        };
    }

    let graph = LouvainGraph::from_indexed(n, adj);
    let mut total_iterations = 0;
    let original_degrees: Vec<f64> = graph.degree.clone();

    let (community, iter1, _) = graph.phase1();
    total_iterations += iter1;

    let mut level_assignment = community;
    let mut current_graph = graph;

    for _level in 0..10 {
        let coarse = current_graph.coarsen(&level_assignment);
        if coarse.n == current_graph.n {
            break;
        }
        let (coarse_community, iters, improved) = coarse.phase1();
        total_iterations += iters;
        if !improved {
            break;
        }

        let mut remap = HashMap::new();
        for &c in &level_assignment {
            if !remap.contains_key(&c) {
                let idx = remap.len();
                remap.insert(c, idx);
            }
        }

        let mut final_community = vec![0usize; n];
        for i in 0..n {
            let coarse_node = remap[&level_assignment[i]];
            final_community[i] = coarse_community[coarse_node];
        }

        let mut final_remap = HashMap::new();
        let mut next_id = 0usize;
        for c in &final_community {
            if let Some(&_id) = final_remap.get(c) {
                continue;
            }
            final_remap.insert(*c, next_id);
            next_id += 1;
        }
        for i in 0..n {
            final_community[i] = final_remap[&final_community[i]];
        }

        level_assignment = final_community;
        current_graph = coarse;
    }

    let community = level_assignment;

    let mut node_to_comm: HashMap<String, usize> = HashMap::new();
    for (i, &c) in community.iter().enumerate() {
        node_to_comm.insert(node_vec[i].clone(), c);
    }

    let mut comm_members: HashMap<usize, Vec<String>> = HashMap::new();
    let mut comm_internal: HashMap<usize, f64> = HashMap::new();
    let mut comm_degree_map: HashMap<usize, f64> = HashMap::new();

    for (i, &c) in community.iter().enumerate() {
        comm_members.entry(c).or_default().push(node_vec[i].clone());
        *comm_degree_map.entry(c).or_insert(0.0) += original_degrees[i];
    }
    for (a, b) in edges {
        let ca = node_to_comm[a];
        let cb = node_to_comm[b];
        if ca == cb {
            *comm_internal.entry(ca).or_insert(0.0) += 1.0;
        }
    }

    let mut total_modularity = 0.0;
    let mut communities: Vec<Community> = comm_members
        .into_iter()
        .map(|(id, mut members)| {
            members.sort();
            let lc = comm_internal.get(&id).copied().unwrap_or(0.0);
            let dc = comm_degree_map[&id];
            let mod_contrib = lc / m - (dc / (2.0 * m)).powi(2);
            total_modularity += mod_contrib;
            Community {
                id,
                members: members.into_iter().map(CommunityMember::new).collect(),
                modularity_contribution: mod_contrib,
            }
        })
        .collect();

    communities.sort_by(|a, b| b.members.len().cmp(&a.members.len()).then(a.id.cmp(&b.id)));

    CommunityResult {
        communities,
        modularity: total_modularity,
        iterations: total_iterations,
        node_count: n,
        edge_count: m as usize,
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct PathNode {
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub tagpath_handle: Option<String>,
}

impl PathNode {
    pub fn new(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            tagpath_handle: None,
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct PathResult {
    pub from: String,
    pub to: String,
    pub path: Vec<PathNode>,
    pub hops: usize,
}

pub fn shortest_path(edges: &[(String, String)], from: &str, to: &str) -> Option<PathResult> {
    if from == to {
        return Some(PathResult {
            from: from.to_string(),
            to: to.to_string(),
            path: vec![PathNode::new(from)],
            hops: 0,
        });
    }

    let mut adj: HashMap<&str, HashSet<&str>> = HashMap::new();
    for (a, b) in edges {
        if a == b {
            continue;
        }
        adj.entry(a.as_str()).or_default().insert(b.as_str());
        adj.entry(b.as_str()).or_default().insert(a.as_str());
    }

    if !adj.contains_key(from) || !adj.contains_key(to) {
        return None;
    }

    let mut visited: HashSet<&str> = HashSet::new();
    let mut queue: VecDeque<&str> = VecDeque::new();
    let mut parent: HashMap<&str, &str> = HashMap::new();

    visited.insert(from);
    queue.push_back(from);

    while let Some(current) = queue.pop_front() {
        if let Some(neighbors) = adj.get(current) {
            for &neighbor in neighbors {
                if visited.insert(neighbor) {
                    parent.insert(neighbor, current);
                    if neighbor == to {
                        let mut path = vec![PathNode::new(to)];
                        let mut curr = to;
                        while let Some(&p) = parent.get(curr) {
                            path.push(PathNode::new(p));
                            curr = p;
                        }
                        path.reverse();
                        let hops = path.len() - 1;
                        return Some(PathResult {
                            from: from.to_string(),
                            to: to.to_string(),
                            path,
                            hops,
                        });
                    }
                    queue.push_back(neighbor);
                }
            }
        }
    }

    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(feature = "lang-rust")]
    #[test]
    fn rust_direct_call() {
        let source = b"fn helper() {}\nfn main() { helper(); }";
        let sites = extract_call_sites(Lang::Rust, source).unwrap();
        assert!(
            sites.iter().any(|s| s.callee == "helper"),
            "got: {:?}",
            sites
        );
    }

    #[cfg(feature = "lang-rust")]
    #[test]
    fn rust_method_call() {
        let source = b"fn main() { vec.push(1); }";
        let sites = extract_call_sites(Lang::Rust, source).unwrap();
        assert!(sites.iter().any(|s| s.callee == "push"), "got: {:?}", sites);
    }

    #[cfg(feature = "lang-rust")]
    #[test]
    fn rust_scoped_call() {
        let source = b"fn main() { Vec::new(); }";
        let sites = extract_call_sites(Lang::Rust, source).unwrap();
        assert!(sites.iter().any(|s| s.callee == "new"), "got: {:?}", sites);
    }

    #[cfg(feature = "lang-rust")]
    #[test]
    fn rust_macro_call() {
        let source = b"fn main() { println!(\"hi\"); }";
        let sites = extract_call_sites(Lang::Rust, source).unwrap();
        assert!(
            sites.iter().any(|s| s.callee == "println"),
            "got: {:?}",
            sites
        );
    }

    #[cfg(feature = "lang-rust")]
    #[test]
    fn rust_axum_route_extracted() {
        let source = br#"fn router() {
    Router::new().route("/users", get(list_users));
}
fn list_users() {}
"#;
        let routes = extract_route_sites(Lang::Rust, source).unwrap();
        assert!(routes.iter().any(|route| {
            route.framework == "axum"
                && route.method.as_deref() == Some("get")
                && route.path == "/users"
                && route.handler == "list_users"
        }));
    }

    #[cfg(feature = "lang-rust")]
    #[test]
    fn rust_actix_route_attribute_extracted() {
        let source = br#"#[post("/submit")]
async fn submit_form() {}
"#;
        let routes = extract_route_sites(Lang::Rust, source).unwrap();
        assert_eq!(routes.len(), 1);
        assert_eq!(routes[0].framework, "actix");
        assert_eq!(routes[0].method.as_deref(), Some("post"));
        assert_eq!(routes[0].handler, "submit_form");
    }

    #[cfg(feature = "lang-python")]
    #[test]
    fn python_direct_call() {
        let source = b"def helper(): pass\ndef main(): helper()";
        let sites = extract_call_sites(Lang::Python, source).unwrap();
        assert!(
            sites.iter().any(|s| s.callee == "helper"),
            "got: {:?}",
            sites
        );
    }

    #[cfg(feature = "lang-python")]
    #[test]
    fn python_method_call() {
        let source = b"def main(): obj.method()";
        let sites = extract_call_sites(Lang::Python, source).unwrap();
        assert!(
            sites.iter().any(|s| s.callee == "method"),
            "got: {:?}",
            sites
        );
    }

    #[cfg(feature = "lang-python")]
    #[test]
    fn python_fastapi_route_extracted() {
        let source = br#"@router.get("/items/{item_id}")
def read_item(item_id: str):
    return item_id
"#;
        let routes = extract_route_sites(Lang::Python, source).unwrap();
        assert_eq!(routes.len(), 1);
        assert_eq!(routes[0].framework, "fastapi");
        assert_eq!(routes[0].method.as_deref(), Some("get"));
        assert_eq!(routes[0].path, "/items/{item_id}");
        assert_eq!(routes[0].handler, "read_item");
    }

    #[cfg(feature = "lang-typescript")]
    #[test]
    fn typescript_direct_call() {
        let source = b"function helper() {}\nfunction main() { helper(); }";
        let sites = extract_call_sites(Lang::TypeScript, source).unwrap();
        assert!(
            sites.iter().any(|s| s.callee == "helper"),
            "got: {:?}",
            sites
        );
    }

    #[cfg(feature = "lang-typescript")]
    #[test]
    fn typescript_method_call() {
        let source = b"function main() { arr.push(1); }";
        let sites = extract_call_sites(Lang::TypeScript, source).unwrap();
        assert!(sites.iter().any(|s| s.callee == "push"), "got: {:?}", sites);
    }

    #[cfg(feature = "lang-typescript")]
    #[test]
    fn typescript_express_route_extracted() {
        let source = br#"router.post("/users", createUser);
function createUser() {}
"#;
        let routes = extract_route_sites(Lang::TypeScript, source).unwrap();
        assert_eq!(routes.len(), 1);
        assert_eq!(routes[0].framework, "express");
        assert_eq!(routes[0].method.as_deref(), Some("post"));
        assert_eq!(routes[0].path, "/users");
        assert_eq!(routes[0].handler, "createUser");
    }

    #[cfg(feature = "lang-javascript")]
    #[test]
    fn javascript_call() {
        let source = b"function main() { helper(); obj.method(); }";
        let sites = extract_call_sites(Lang::JavaScript, source).unwrap();
        assert!(
            sites.iter().any(|s| s.callee == "helper"),
            "got: {:?}",
            sites
        );
        assert!(
            sites.iter().any(|s| s.callee == "method"),
            "got: {:?}",
            sites
        );
    }

    #[cfg(feature = "lang-rust")]
    #[test]
    fn resolve_edges_basic() {
        let symbols = vec![
            Symbol {
                name: "main".into(),
                kind: "function".into(),
                line: 1,
                end_line: 3,
            },
            Symbol {
                name: "helper".into(),
                kind: "function".into(),
                line: 5,
                end_line: 7,
            },
        ];
        let sites = vec![
            CallSite {
                callee: "helper".into(),
                line: 2,
            },
            CallSite {
                callee: "println".into(),
                line: 6,
            },
        ];
        let edges = resolve_edges(&symbols, &sites);
        assert_eq!(edges.len(), 2);
        assert_eq!(edges[0].caller, "main");
        assert_eq!(edges[0].callee, "helper");
        assert_eq!(edges[1].caller, "helper");
        assert_eq!(edges[1].callee, "println");
    }

    #[cfg(feature = "lang-rust")]
    #[test]
    fn resolve_edges_nested_picks_innermost() {
        let symbols = vec![
            Symbol {
                name: "outer".into(),
                kind: "function".into(),
                line: 0,
                end_line: 10,
            },
            Symbol {
                name: "inner".into(),
                kind: "function".into(),
                line: 2,
                end_line: 5,
            },
        ];
        let sites = vec![CallSite {
            callee: "foo".into(),
            line: 3,
        }];
        let edges = resolve_edges(&symbols, &sites);
        assert_eq!(edges.len(), 1);
        assert_eq!(edges[0].caller, "inner");
    }

    #[cfg(feature = "lang-rust")]
    #[test]
    fn resolve_edges_top_level_call_excluded() {
        let symbols = vec![Symbol {
            name: "main".into(),
            kind: "function".into(),
            line: 5,
            end_line: 10,
        }];
        let sites = vec![CallSite {
            callee: "foo".into(),
            line: 2,
        }];
        let edges = resolve_edges(&symbols, &sites);
        assert!(edges.is_empty());
    }

    #[test]
    fn resolve_edges_cache_reuses_slots_until_mtime_or_hash_changes() {
        let cache = ResolveEdgesCache::new();
        let file = std::path::Path::new("src/lib.rs");
        let symbols = vec![Symbol {
            name: "main".into(),
            kind: "function".into(),
            line: 1,
            end_line: 3,
        }];
        let sites = vec![CallSite {
            callee: "helper".into(),
            line: 2,
        }];

        let first =
            cache.resolve_edges_for_file(file, "hash-a", FileMtime::new(10, 0), &symbols, &sites);
        assert_eq!(first.len(), 1);
        assert_eq!(cache.stats(), (0, 1));

        let cached =
            cache.resolve_edges_for_file(file, "hash-a", FileMtime::new(10, 0), &symbols, &sites);
        assert_eq!(cached, first);
        assert_eq!(cache.stats(), (1, 1));

        let refreshed =
            cache.resolve_edges_for_file(file, "hash-a", FileMtime::new(11, 0), &symbols, &sites);
        assert_eq!(refreshed, first);
        assert_eq!(cache.stats(), (1, 2));

        let new_hash =
            cache.resolve_edges_for_file(file, "hash-b", FileMtime::new(11, 0), &symbols, &sites);
        assert_eq!(new_hash, first);
        assert_eq!(cache.stats(), (1, 3));
    }

    #[test]
    fn project_call_edges_to_provider_neutral_substrate() {
        let edges = vec![CallEdge {
            caller: "main".into(),
            callee: "helper".into(),
            caller_line: 10,
            call_site_line: 12,
        }];
        let projection = project_call_edges(
            &edges,
            Some(GraphProvenance::new("tsift.index", "src/main.rs")),
        );

        assert_eq!(projection.nodes.len(), 2);
        assert_eq!(projection.edges.len(), 1);
        assert!(
            projection
                .nodes
                .iter()
                .any(|node| node.id == code_symbol_node_id("main") && node.kind == "code_symbol")
        );
    }

    #[test]
    fn project_routes_to_provider_neutral_substrate() {
        let routes = vec![RouteSite {
            framework: "fastapi".into(),
            method: Some("get".into()),
            path: "/items".into(),
            handler: "list_items".into(),
            line: 3,
            handler_line: Some(4),
        }];
        let projection = project_routes(
            &routes,
            Some(GraphProvenance::new("tsift.index", "src/api.py")),
        );

        assert!(
            projection
                .nodes
                .iter()
                .any(|node| node.kind == "route" && node.label == "GET /items")
        );
        assert!(projection.edges.iter().any(|edge| edge.kind == "handled_by"
            && edge.properties.get("route_path") == Some(&"/items".to_string())));
    }

    #[test]
    fn no_call_query_returns_empty() {
        #[cfg(feature = "lang-markdown")]
        {
            let sites = extract_call_sites(Lang::Markdown, b"# Hello").unwrap();
            assert!(sites.is_empty());
        }
    }

    #[cfg(feature = "lang-rust")]
    #[test]
    fn full_roundtrip_rust() {
        let source = b"fn helper() { println!(\"hi\"); }\nfn main() { helper(); Vec::new(); }";
        let symbols = Lang::Rust.extract_symbols(source).unwrap();
        let sites = extract_call_sites(Lang::Rust, source).unwrap();
        let edges = resolve_edges(&symbols, &sites);
        let main_calls: Vec<&str> = edges
            .iter()
            .filter(|e| e.caller == "main")
            .map(|e| e.callee.as_str())
            .collect();
        assert!(
            main_calls.contains(&"helper"),
            "main should call helper, got: {:?}",
            main_calls
        );
        assert!(
            main_calls.contains(&"new"),
            "main should call new, got: {:?}",
            main_calls
        );
    }

    fn s(a: &str, b: &str) -> (String, String) {
        (a.to_string(), b.to_string())
    }

    #[test]
    fn communities_empty_graph() {
        let result = detect_communities(&[]);
        assert_eq!(result.node_count, 0);
        assert_eq!(result.edge_count, 0);
        assert!(result.communities.is_empty());
        assert_eq!(result.iterations, 0);
    }

    #[test]
    fn communities_single_edge() {
        let edges = vec![s("a", "b")];
        let result = detect_communities(&edges);
        assert_eq!(result.node_count, 2);
        assert_eq!(result.edge_count, 1);
        assert_eq!(result.communities.len(), 1);
        assert_eq!(result.communities[0].members.len(), 2);
    }

    #[test]
    fn communities_self_loop_ignored() {
        let edges = vec![s("a", "a"), s("a", "b")];
        let result = detect_communities(&edges);
        assert_eq!(result.node_count, 2);
        assert_eq!(result.edge_count, 1);
    }

    #[test]
    fn communities_duplicate_edges_deduplicated() {
        let edges = vec![
            s("main", "helper"),
            s("main", "helper"),
            s("main", "helper"),
        ];
        let result = detect_communities(&edges);
        assert_eq!(result.node_count, 2);
        assert_eq!(result.edge_count, 1);
    }

    #[test]
    fn communities_two_cliques_split() {
        let edges = vec![
            s("a", "b"),
            s("a", "c"),
            s("b", "c"),
            s("d", "e"),
            s("d", "f"),
            s("e", "f"),
            s("a", "d"),
        ];
        let result = detect_communities(&edges);
        assert_eq!(result.node_count, 6);
        assert_eq!(
            result.communities.len(),
            2,
            "expected 2 communities, got: {:?}",
            result
                .communities
                .iter()
                .map(|c| &c.members)
                .collect::<Vec<_>>()
        );
        assert_eq!(result.communities[0].members.len(), 3);
        assert_eq!(result.communities[1].members.len(), 3);
        assert!(result.modularity > 0.0);
    }

    #[test]
    fn communities_disconnected_components() {
        let edges = vec![s("a", "b"), s("c", "d")];
        let result = detect_communities(&edges);
        assert_eq!(result.node_count, 4);
        assert_eq!(result.edge_count, 2);
        assert!(result.modularity >= 0.0);
    }

    #[test]
    fn communities_modularity_non_negative_for_clustered() {
        let edges = vec![
            s("a", "b"),
            s("a", "c"),
            s("b", "c"),
            s("d", "e"),
            s("d", "f"),
            s("e", "f"),
        ];
        let result = detect_communities(&edges);
        assert!(result.modularity >= 0.0, "Q={}", result.modularity);
    }

    #[test]
    fn communities_hierarchical_phase2_improves_modularity() {
        let mut edges = Vec::new();
        for cluster in 0..4 {
            let base = cluster * 6;
            for i in 0..6 {
                for j in (i + 1)..6 {
                    edges.push((format!("c{}n{}", cluster, base + i), format!("c{}n{}", cluster, base + j)));
                }
            }
        }
        edges.push(("c0n0".to_string(), "c1n6".to_string()));
        edges.push(("c2n12".to_string(), "c3n18".to_string()));
        edges.push(("c0n1".to_string(), "c2n12".to_string()));

        let result = detect_communities(&edges);
        assert!(result.modularity > 0.0, "Q={}", result.modularity);
        assert!(
            result.communities.len() >= 2,
            "expected >= 2 communities for hierarchical structure, got {}",
            result.communities.len()
        );
        assert!(result.iterations >= 1);
    }

    fn path_names(result: &PathResult) -> Vec<&str> {
        result.path.iter().map(|n| n.name.as_str()).collect()
    }

    #[test]
    fn path_direct_neighbors() {
        let edges = vec![s("a", "b")];
        let result = shortest_path(&edges, "a", "b").unwrap();
        assert_eq!(path_names(&result), vec!["a", "b"]);
        assert_eq!(result.hops, 1);
        assert!(result.path.iter().all(|n| n.tagpath_handle.is_none()));
    }

    #[test]
    fn path_two_hops() {
        let edges = vec![s("a", "b"), s("b", "c")];
        let result = shortest_path(&edges, "a", "c").unwrap();
        assert_eq!(result.hops, 2);
        assert_eq!(result.path.first().unwrap().name, "a");
        assert_eq!(result.path.last().unwrap().name, "c");
    }

    #[test]
    fn path_same_node() {
        let edges = vec![s("a", "b")];
        let result = shortest_path(&edges, "a", "a").unwrap();
        assert_eq!(path_names(&result), vec!["a"]);
        assert_eq!(result.hops, 0);
    }

    #[test]
    fn path_no_connection() {
        let edges = vec![s("a", "b"), s("c", "d")];
        assert!(shortest_path(&edges, "a", "c").is_none());
    }

    #[test]
    fn path_unknown_node() {
        let edges = vec![s("a", "b")];
        assert!(shortest_path(&edges, "a", "z").is_none());
    }

    #[test]
    fn path_prefers_shorter() {
        let edges = vec![s("a", "b"), s("b", "c"), s("a", "c")];
        let result = shortest_path(&edges, "a", "c").unwrap();
        assert_eq!(result.hops, 1);
    }

    #[test]
    fn path_self_loop_ignored() {
        let edges = vec![s("a", "a"), s("a", "b")];
        let result = shortest_path(&edges, "a", "b").unwrap();
        assert_eq!(result.hops, 1);
    }

    #[test]
    fn terse_community_drops_optional_fields() {
        let member = CommunityMember {
            name: "foo".to_string(),
            file: Some("src/lib.rs".to_string()),
            line: Some(42),
            refs: vec![CommunityMemberRef {
                file: "src/lib.rs".to_string(),
                line: 42,
                role: "call".to_string(),
                peer: "bar".to_string(),
            }],
            tagpath_handle: Some("foo::lib".to_string()),
        };
        let terse = TerseCommunityMember::from(&member);
        assert_eq!(terse.name, "foo");
        assert_eq!(terse.tagpath_handle, Some("foo::lib".to_string()));
    }

    #[test]
    fn terse_community_top_n_truncates_members() {
        let community = Community {
            id: 0,
            members: vec![
                CommunityMember::new("a"),
                CommunityMember::new("b"),
                CommunityMember::new("c"),
                CommunityMember::new("d"),
                CommunityMember::new("e"),
            ],
            modularity_contribution: 0.25,
        };
        let terse = TerseCommunity::from_community(&community, 3);
        assert_eq!(terse.id, 0);
        assert_eq!(terse.members.len(), 3);
        assert_eq!(terse.members[0].name, "a");
        assert_eq!(terse.members[2].name, "c");
        assert_eq!(terse.modularity_contribution, 0.25);
    }

    #[test]
    fn terse_community_result_from_detect_communities() {
        let edges = vec![s("a", "b"), s("b", "c"), s("c", "d")];
        let result = detect_communities(&edges);
        let terse = result.to_terse(2);
        assert_eq!(terse.node_count, result.node_count);
        assert_eq!(terse.edge_count, result.edge_count);
        assert_eq!(terse.modularity, result.modularity);
        assert_eq!(terse.communities.len(), result.communities.len());
        for tc in &terse.communities {
            assert!(tc.members.len() <= 2);
        }
    }

    #[test]
    fn terse_community_json_smaller_than_full() {
        let edges: Vec<(String, String)> = (0..20)
            .flat_map(|i| {
                let base = i * 5;
                vec![
                    (format!("n{}", base), format!("n{}", base + 1)),
                    (format!("n{}", base), format!("n{}", base + 2)),
                    (format!("n{}", base + 1), format!("n{}", base + 2)),
                    (format!("n{}", base + 2), format!("n{}", base + 3)),
                    (format!("n{}", base + 3), format!("n{}", base + 4)),
                ]
            })
            .chain(std::iter::once(("n0".to_string(), "n5".to_string())))
            .collect();
        let result = detect_communities(&edges);
        let terse = result.to_terse(2);
        let full_member_count: usize = result.communities.iter().map(|c| c.members.len()).sum();
        let terse_member_count: usize = terse.communities.iter().map(|c| c.members.len()).sum();
        assert!(
            terse_member_count < full_member_count,
            "terse members ({}) should be fewer than full ({})",
            terse_member_count,
            full_member_count
        );
    }
}
