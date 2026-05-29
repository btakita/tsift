use serde::{Deserialize, Serialize};
use std::collections::HashMap;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SccComponent {
    pub nodes: Vec<String>,
    pub size: usize,
    pub is_trivial: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SccResult {
    pub components: Vec<SccComponent>,
    pub total_components: usize,
    pub non_trivial_count: usize,
    pub largest_component_size: usize,
    pub node_count: usize,
    pub edge_count: usize,
}

pub fn tarjan_scc(edges: &[(String, String)]) -> SccResult {
    if edges.is_empty() {
        return SccResult {
            components: Vec::new(),
            total_components: 0,
            non_trivial_count: 0,
            largest_component_size: 0,
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

    let mut adj: Vec<Vec<usize>> = vec![Vec::new(); n];
    for (a, b) in edges {
        let ai = node_idx[a];
        let bi = node_idx[b];
        adj[ai].push(bi);
    }

    let mut index_counter = 0usize;
    let mut indices = vec![None::<usize>; n];
    let mut lowlinks = vec![0usize; n];
    let mut on_stack = vec![false; n];
    let mut stack: Vec<usize> = Vec::new();
    let mut components: Vec<Vec<usize>> = Vec::new();

    #[allow(clippy::too_many_arguments)]
    fn strongconnect(
        v: usize,
        adj: &[Vec<usize>],
        index_counter: &mut usize,
        indices: &mut Vec<Option<usize>>,
        lowlinks: &mut Vec<usize>,
        on_stack: &mut Vec<bool>,
        stack: &mut Vec<usize>,
        components: &mut Vec<Vec<usize>>,
    ) {
        indices[v] = Some(*index_counter);
        lowlinks[v] = *index_counter;
        *index_counter += 1;
        stack.push(v);
        on_stack[v] = true;

        for &w in &adj[v] {
            match indices[w] {
                None => {
                    strongconnect(w, adj, index_counter, indices, lowlinks, on_stack, stack, components);
                    lowlinks[v] = lowlinks[v].min(lowlinks[w]);
                }
                Some(_) if on_stack[w] => {
                    lowlinks[v] = lowlinks[v].min(indices[w].unwrap());
                }
                _ => {}
            }
        }

        if indices[v] == Some(lowlinks[v]) {
            let mut component = Vec::new();
            loop {
                let w = stack.pop().unwrap();
                on_stack[w] = false;
                component.push(w);
                if w == v {
                    break;
                }
            }
            components.push(component);
        }
    }

    for v in 0..n {
        if indices[v].is_none() {
            strongconnect(
                v, &adj, &mut index_counter, &mut indices, &mut lowlinks,
                &mut on_stack, &mut stack, &mut components,
            );
        }
    }

    let mut scc_components: Vec<SccComponent> = components
        .into_iter()
        .map(|indices| {
            let mut nodes: Vec<String> = indices.iter().map(|&i| node_vec[i].clone()).collect();
            nodes.sort();
            let size = nodes.len();
            let is_trivial = size == 1 && {
                let node_idx_val = node_idx[&nodes[0]];
                !adj[node_idx_val].contains(&node_idx_val)
            };
            SccComponent {
                is_trivial,
                nodes,
                size,
            }
        })
        .collect();

    let non_trivial_count = scc_components.iter().filter(|c| !c.is_trivial).count();
    let largest_component_size = scc_components.iter().map(|c| c.size).max().unwrap_or(0);

    scc_components.sort_by(|a, b| b.size.cmp(&a.size).then(a.nodes.cmp(&b.nodes)));

    SccResult {
        total_components: scc_components.len(),
        non_trivial_count,
        largest_component_size,
        components: scc_components,
        node_count: n,
        edge_count: edges.len(),
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
        let result = tarjan_scc(&[]);
        assert_eq!(result.total_components, 0);
        assert_eq!(result.node_count, 0);
    }

    #[test]
    fn single_node_no_edges() {
        let result = tarjan_scc(&[]);
        assert_eq!(result.total_components, 0);
    }

    #[test]
    fn single_edge_two_components() {
        let edges = vec![e("a", "b")];
        let result = tarjan_scc(&edges);
        assert_eq!(result.total_components, 2);
        assert!(result.components.iter().all(|c| c.is_trivial));
    }

    #[test]
    fn simple_cycle() {
        let edges = vec![e("a", "b"), e("b", "c"), e("c", "a")];
        let result = tarjan_scc(&edges);
        assert_eq!(result.non_trivial_count, 1);
        assert_eq!(result.largest_component_size, 3);
        let scc = &result.components[0];
        assert_eq!(scc.nodes, vec!["a", "b", "c"]);
    }

    #[test]
    fn two_cycles_connected() {
        let edges = vec![
            e("a", "b"),
            e("b", "a"),
            e("c", "d"),
            e("d", "c"),
            e("a", "c"),
        ];
        let result = tarjan_scc(&edges);
        assert_eq!(result.non_trivial_count, 2);
        assert_eq!(result.total_components, 2);
    }

    #[test]
    fn self_loop_is_nontrivial() {
        let edges = vec![e("a", "a")];
        let result = tarjan_scc(&edges);
        assert_eq!(result.non_trivial_count, 1);
        assert_eq!(result.components[0].nodes, vec!["a"]);
        assert!(!result.components[0].is_trivial);
    }

    #[test]
    fn dag_all_trivial() {
        let edges = vec![e("a", "b"), e("b", "c"), e("a", "c")];
        let result = tarjan_scc(&edges);
        assert_eq!(result.non_trivial_count, 0);
        assert_eq!(result.total_components, 3);
    }

    #[test]
    fn nested_cycles() {
        let edges = vec![
            e("a", "b"),
            e("b", "c"),
            e("c", "a"),
            e("b", "d"),
            e("d", "b"),
        ];
        let result = tarjan_scc(&edges);
        assert_eq!(result.non_trivial_count, 1);
        assert_eq!(result.largest_component_size, 4);
    }
}
