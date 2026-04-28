use crate::lang::{Lang, Symbol};
use anyhow::Result;
use serde::Serialize;
use std::collections::{HashMap, HashSet, VecDeque};
use tree_sitter::{Parser, Query, QueryCursor, StreamingIterator};

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

impl Lang {
    pub fn call_query(&self) -> Option<&'static str> {
        match self {
            #[cfg(feature = "lang-rust")]
            Self::Rust => Some(
                r#"
                (call_expression function: (identifier) @call.name)
                (call_expression function: (field_expression field: (field_identifier) @call.name))
                (call_expression function: (scoped_identifier name: (identifier) @call.name))
                (macro_invocation macro: (identifier) @call.name)
            "#,
            ),
            #[cfg(feature = "lang-python")]
            Self::Python => Some(
                r#"
                (call function: (identifier) @call.name)
                (call function: (attribute attribute: (identifier) @call.name))
            "#,
            ),
            #[cfg(feature = "lang-typescript")]
            Self::TypeScript | Self::Tsx => Some(
                r#"
                (call_expression function: (identifier) @call.name)
                (call_expression function: (member_expression property: (property_identifier) @call.name))
            "#,
            ),
            #[cfg(feature = "lang-javascript")]
            Self::JavaScript | Self::Jsx => Some(
                r#"
                (call_expression function: (identifier) @call.name)
                (call_expression function: (member_expression property: (property_identifier) @call.name))
            "#,
            ),
            #[cfg(feature = "lang-kotlin")]
            Self::Kotlin => Some(
                r#"
                (call_expression (simple_identifier) @call.name)
            "#,
            ),
            _ => None,
        }
    }
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

pub fn resolve_edges(symbols: &[Symbol], call_sites: &[CallSite]) -> Vec<CallEdge> {
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

#[derive(Debug, Clone, Serialize)]
pub struct Community {
    pub id: usize,
    pub members: Vec<String>,
    pub modularity_contribution: f64,
}

#[derive(Debug, Clone, Serialize)]
pub struct CommunityResult {
    pub communities: Vec<Community>,
    pub modularity: f64,
    pub iterations: usize,
    pub node_count: usize,
    pub edge_count: usize,
}

/// Louvain community detection over an undirected call graph.
///
/// Edges are treated as undirected; self-loops and parallel edges are collapsed.
/// Returns communities sorted largest-first with total modularity Q.
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

    // Collect nodes in first-appearance order
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

    // Build undirected adjacency (no self-loops, deduplicated)
    let mut adj: Vec<HashSet<usize>> = vec![HashSet::new(); n];
    for (a, b) in edges {
        let ai = node_idx[a];
        let bi = node_idx[b];
        if ai != bi {
            adj[ai].insert(bi);
            adj[bi].insert(ai);
        }
    }

    let degree: Vec<f64> = adj.iter().map(|nb| nb.len() as f64).collect();
    let m = degree.iter().sum::<f64>() / 2.0;

    if m == 0.0 {
        let communities = node_vec
            .iter()
            .enumerate()
            .map(|(i, name)| Community {
                id: i,
                members: vec![name.clone()],
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

    // Phase 1: greedy local modularity optimization
    // Each node starts in its own community; comm_degree[c] = sum of degrees in c.
    let mut community: Vec<usize> = (0..n).collect();
    let mut comm_degree: Vec<f64> = degree.clone();

    let mut iterations = 0;
    loop {
        let mut improved = false;
        iterations += 1;

        for i in 0..n {
            let cur_c = community[i];
            let ki = degree[i];

            // Gain of keeping i in cur_c (vs. isolated) — used as baseline for comparison
            let ki_in_cur = adj[i].iter().filter(|&&nb| community[nb] == cur_c).count() as f64;
            let cur_gain = ki_in_cur / m - ki * (comm_degree[cur_c] - ki) / (2.0 * m * m);

            let mut best_delta = 0.0f64;
            let mut best_c = cur_c;

            // Candidates: communities reachable via neighbors (excluding current)
            let mut seen_comms: HashSet<usize> = HashSet::new();
            for &nb in &adj[i] {
                let c = community[nb];
                if c == cur_c || !seen_comms.insert(c) {
                    continue;
                }
                let ki_in_c = adj[i].iter().filter(|&&nb2| community[nb2] == c).count() as f64;
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
                community[i] = best_c;
                improved = true;
            }
        }

        if !improved || iterations >= 100 {
            break;
        }
    }

    // Collect community members and count internal edges
    let mut comm_members: HashMap<usize, Vec<String>> = HashMap::new();
    let mut comm_internal: HashMap<usize, f64> = HashMap::new();

    for (i, &c) in community.iter().enumerate() {
        comm_members.entry(c).or_default().push(node_vec[i].clone());
    }
    for i in 0..n {
        for &nb in &adj[i] {
            if nb > i && community[nb] == community[i] {
                *comm_internal.entry(community[i]).or_insert(0.0) += 1.0;
            }
        }
    }

    let mut total_modularity = 0.0;
    let mut communities: Vec<Community> = comm_members
        .into_iter()
        .map(|(id, mut members)| {
            members.sort();
            let lc = comm_internal.get(&id).copied().unwrap_or(0.0);
            let dc = comm_degree[id];
            let mod_contrib = lc / m - (dc / (2.0 * m)).powi(2);
            total_modularity += mod_contrib;
            Community {
                id,
                members,
                modularity_contribution: mod_contrib,
            }
        })
        .collect();

    communities.sort_by(|a, b| b.members.len().cmp(&a.members.len()).then(a.id.cmp(&b.id)));

    CommunityResult {
        communities,
        modularity: total_modularity,
        iterations,
        node_count: n,
        edge_count: m as usize,
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct PathResult {
    pub from: String,
    pub to: String,
    pub path: Vec<String>,
    pub hops: usize,
}

pub fn shortest_path(edges: &[(String, String)], from: &str, to: &str) -> Option<PathResult> {
    if from == to {
        return Some(PathResult {
            from: from.to_string(),
            to: to.to_string(),
            path: vec![from.to_string()],
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
                        let mut path = vec![to.to_string()];
                        let mut curr = to;
                        while let Some(&p) = parent.get(curr) {
                            path.push(p.to_string());
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

    // --- detect_communities ---

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
        // Both nodes should be in the same community
        assert_eq!(result.communities.len(), 1);
        assert_eq!(result.communities[0].members.len(), 2);
    }

    #[test]
    fn communities_self_loop_ignored() {
        let edges = vec![s("a", "a"), s("a", "b")];
        let result = detect_communities(&edges);
        assert_eq!(result.node_count, 2);
        assert_eq!(result.edge_count, 1); // self-loop excluded
    }

    #[test]
    fn communities_duplicate_edges_deduplicated() {
        // Multiple call sites between the same functions = one undirected edge
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
        // Two tight cliques with a single bridge: Louvain should split them
        let edges = vec![
            s("a", "b"),
            s("a", "c"),
            s("b", "c"), // clique 1
            s("d", "e"),
            s("d", "f"),
            s("e", "f"), // clique 2
            s("a", "d"), // bridge
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

    // --- shortest_path ---

    #[test]
    fn path_direct_neighbors() {
        let edges = vec![s("a", "b")];
        let result = shortest_path(&edges, "a", "b").unwrap();
        assert_eq!(result.path, vec!["a", "b"]);
        assert_eq!(result.hops, 1);
    }

    #[test]
    fn path_two_hops() {
        let edges = vec![s("a", "b"), s("b", "c")];
        let result = shortest_path(&edges, "a", "c").unwrap();
        assert_eq!(result.hops, 2);
        assert_eq!(result.path.first().unwrap(), "a");
        assert_eq!(result.path.last().unwrap(), "c");
    }

    #[test]
    fn path_same_node() {
        let edges = vec![s("a", "b")];
        let result = shortest_path(&edges, "a", "a").unwrap();
        assert_eq!(result.path, vec!["a"]);
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
}
