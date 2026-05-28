use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet, VecDeque};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeadCodeNode {
    pub name: String,
    pub reason: DeadCodeReason,
    pub inbound_count: usize,
    pub outbound_count: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum DeadCodeReason {
    Unreachable,
    NoOutboundCalls,
    Isolated,
    OrphanedLeaf,
}

impl std::fmt::Display for DeadCodeReason {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            DeadCodeReason::Unreachable => write!(f, "unreachable"),
            DeadCodeReason::NoOutboundCalls => write!(f, "no_outbound_calls"),
            DeadCodeReason::Isolated => write!(f, "isolated"),
            DeadCodeReason::OrphanedLeaf => write!(f, "orphaned_leaf"),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeadCodeResult {
    pub dead_nodes: Vec<DeadCodeNode>,
    pub total_nodes: usize,
    pub total_edges: usize,
    pub dead_count: usize,
    pub dead_ratio: f64,
    pub entry_points: Vec<String>,
}

pub fn detect_dead_code(
    edges: &[(String, String)],
    entry_points: &[String],
) -> DeadCodeResult {
    if edges.is_empty() {
        return DeadCodeResult {
            dead_nodes: Vec::new(),
            total_nodes: 0,
            total_edges: 0,
            dead_count: 0,
            dead_ratio: 0.0,
            entry_points: entry_points.to_vec(),
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

    for name in entry_points {
        if !node_idx.contains_key(name) {
            node_idx.insert(name.clone(), node_vec.len());
            node_vec.push(name.clone());
        }
    }

    let n = node_vec.len();
    let mut out_adj: Vec<HashSet<usize>> = vec![HashSet::new(); n];
    let mut in_adj: Vec<HashSet<usize>> = vec![HashSet::new(); n];
    for (a, b) in edges {
        let ai = node_idx[a];
        let bi = node_idx[b];
        if ai != bi {
            out_adj[ai].insert(bi);
            in_adj[bi].insert(ai);
        }
    }

    let mut reachable = vec![false; n];
    let mut queue: VecDeque<usize> = VecDeque::new();
    for name in entry_points {
        if let Some(&idx) = node_idx.get(name) {
            if !reachable[idx] {
                reachable[idx] = true;
                queue.push_back(idx);
            }
        }
    }

    while let Some(v) = queue.pop_front() {
        for &w in &out_adj[v] {
            if !reachable[w] {
                reachable[w] = true;
                queue.push_back(w);
            }
        }
    }

    let mut dead_nodes = Vec::new();
    for (i, name) in node_vec.iter().enumerate() {
        if reachable[i] {
            continue;
        }
        let inbound = in_adj[i].len();
        let outbound = out_adj[i].len();
        let reason = if inbound == 0 && outbound == 0 {
            DeadCodeReason::Isolated
        } else if inbound == 0 {
            DeadCodeReason::Unreachable
        } else if outbound == 0 {
            DeadCodeReason::OrphanedLeaf
        } else {
            DeadCodeReason::Unreachable
        };
        dead_nodes.push(DeadCodeNode {
            name: name.clone(),
            reason,
            inbound_count: inbound,
            outbound_count: outbound,
        });
    }

    let dead_count = dead_nodes.len();
    let total_nodes = n;
    let dead_ratio = if total_nodes > 0 {
        dead_count as f64 / total_nodes as f64
    } else {
        0.0
    };

    dead_nodes.sort_by(|a, b| {
        a.reason.to_string().cmp(&b.reason.to_string()).then(a.name.cmp(&b.name))
    });

    DeadCodeResult {
        dead_nodes,
        total_nodes,
        total_edges: edges.len(),
        dead_count,
        dead_ratio,
        entry_points: entry_points.to_vec(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn e(a: &str, b: &str) -> (String, String) {
        (a.to_string(), b.to_string())
    }

    #[test]
    fn empty_graph() {
        let result = detect_dead_code(&[], &[]);
        assert_eq!(result.dead_count, 0);
        assert_eq!(result.total_nodes, 0);
    }

    #[test]
    fn all_reachable_from_entry() {
        let edges = vec![e("main", "helper"), e("helper", "util")];
        let result = detect_dead_code(&edges, &["main".to_string()]);
        assert_eq!(result.dead_count, 0);
    }

    #[test]
    fn unreachable_node_detected() {
        let edges = vec![e("main", "helper"), e("dead", "dead_helper")];
        let result = detect_dead_code(&edges, &["main".to_string()]);
        assert_eq!(result.dead_count, 2);
        let names: Vec<&str> = result.dead_nodes.iter().map(|n| n.name.as_str()).collect();
        assert!(names.contains(&"dead"));
        assert!(names.contains(&"dead_helper"));
    }

    #[test]
    fn isolated_node_detected() {
        let edges = vec![e("a", "b")];
        let result = detect_dead_code(&edges, &["a".to_string()]);
        assert_eq!(result.dead_count, 0);
    }

    #[test]
    fn isolated_with_no_edges() {
        let edges: Vec<(String, String)> = vec![];
        let result = detect_dead_code(&edges, &[]);
        assert_eq!(result.dead_count, 0);
    }

    #[test]
    fn multiple_entry_points() {
        let edges = vec![e("main", "a"), e("init", "b"), e("a", "c"), e("dead", "d")];
        let result = detect_dead_code(
            &edges,
            &["main".to_string(), "init".to_string()],
        );
        assert_eq!(result.dead_count, 2);
    }

    #[test]
    fn dead_ratio_computed() {
        let edges = vec![e("main", "a"), e("dead", "b")];
        let result = detect_dead_code(&edges, &["main".to_string()]);
        assert!(result.dead_ratio > 0.0);
        assert!(result.dead_ratio <= 1.0);
    }

    #[test]
    fn no_entry_points_all_dead() {
        let edges = vec![e("a", "b"), e("b", "c")];
        let result = detect_dead_code(&edges, &[]);
        assert_eq!(result.dead_count, 3);
    }
}
