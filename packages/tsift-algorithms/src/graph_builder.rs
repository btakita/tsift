use std::collections::{HashMap, HashSet};

pub struct Graph {
    pub node_vec: Vec<String>,
    pub node_idx: HashMap<String, usize>,
    pub out_adj: Vec<HashSet<usize>>,
    pub in_adj: Vec<HashSet<usize>>,
}

impl Graph {
    pub fn node_count(&self) -> usize {
        self.node_vec.len()
    }

    pub fn edge_count(&self) -> usize {
        self.out_adj.iter().map(|s| s.len()).sum()
    }

    pub fn ensure_nodes(&mut self, names: &[String]) {
        for name in names {
            if !self.node_idx.contains_key(name) {
                let idx = self.node_vec.len();
                self.node_idx.insert(name.clone(), idx);
                self.node_vec.push(name.clone());
                self.out_adj.push(HashSet::new());
                self.in_adj.push(HashSet::new());
            }
        }
    }
}

pub fn build_graph(edges: &[(String, String)]) -> Graph {
    let (node_vec, node_idx) = build_node_index(edges);
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
    Graph {
        node_vec,
        node_idx,
        out_adj,
        in_adj,
    }
}

pub fn build_node_index(edges: &[(String, String)]) -> (Vec<String>, HashMap<String, usize>) {
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
    (node_vec, node_idx)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn e(a: &str, b: &str) -> (String, String) {
        (a.to_string(), b.to_string())
    }

    #[test]
    fn empty_graph() {
        let g = build_graph(&[]);
        assert_eq!(g.node_count(), 0);
        assert_eq!(g.edge_count(), 0);
    }

    #[test]
    fn single_edge() {
        let g = build_graph(&[e("a", "b")]);
        assert_eq!(g.node_count(), 2);
        assert_eq!(g.edge_count(), 1);
        assert!(g.out_adj[g.node_idx["a"]].contains(&g.node_idx["b"]));
        assert!(g.in_adj[g.node_idx["b"]].contains(&g.node_idx["a"]));
    }

    #[test]
    fn self_loop_excluded() {
        let g = build_graph(&[e("a", "a")]);
        assert_eq!(g.node_count(), 1);
        assert_eq!(g.edge_count(), 0);
    }

    #[test]
    fn build_node_index_basic() {
        let (nodes, idx) = build_node_index(&[e("a", "b"), e("b", "c")]);
        assert_eq!(nodes.len(), 3);
        assert!(idx.contains_key("a"));
        assert!(idx.contains_key("b"));
        assert!(idx.contains_key("c"));
    }

    #[test]
    fn ensure_nodes_adds_missing() {
        let mut g = build_graph(&[e("a", "b")]);
        g.ensure_nodes(&["c".to_string(), "d".to_string()]);
        assert_eq!(g.node_count(), 4);
        assert!(g.node_idx.contains_key("c"));
        assert!(g.node_idx.contains_key("d"));
    }

    #[test]
    fn ensure_nodes_idempotent() {
        let mut g = build_graph(&[e("a", "b")]);
        g.ensure_nodes(&["a".to_string()]);
        assert_eq!(g.node_count(), 2);
    }

    #[test]
    fn multi_edge_graph() {
        let g = build_graph(&[e("a", "b"), e("a", "c"), e("b", "c")]);
        assert_eq!(g.node_count(), 3);
        assert_eq!(g.edge_count(), 3);
    }
}
