use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HealthScore {
    pub name: String,
    pub overall: f64,
    pub connectivity: f64,
    pub reachability: f64,
    pub centrality: f64,
    pub cycle_risk: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HealthReport {
    pub scores: Vec<HealthScore>,
    pub avg_connectivity: f64,
    pub avg_reachability: f64,
    pub avg_centrality: f64,
    pub avg_cycle_risk: f64,
    pub avg_overall: f64,
    pub node_count: usize,
    pub edge_count: usize,
}

#[allow(clippy::type_complexity)]
fn build_graph(edges: &[(String, String)]) -> (Vec<String>, HashMap<String, usize>, Vec<HashSet<usize>>, Vec<HashSet<usize>>) {
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
    (node_vec, node_idx, out_adj, in_adj)
}

fn compute_reachability(n: usize, adj: &[HashSet<usize>]) -> Vec<usize> {
    let mut reach = vec![0usize; n];
    for start in 0..n {
        let mut visited = vec![false; n];
        let mut queue = std::collections::VecDeque::new();
        visited[start] = true;
        queue.push_back(start);
        while let Some(v) = queue.pop_front() {
            for &w in &adj[v] {
                if !visited[w] {
                    visited[w] = true;
                    queue.push_back(w);
                }
            }
        }
        reach[start] = visited.iter().filter(|&&v| v).count();
    }
    reach
}

fn find_cycles(n: usize, adj: &[HashSet<usize>]) -> HashSet<usize> {
    let mut in_cycle = HashSet::new();
    let mut index = 0usize;
    let mut stack = Vec::new();
    let mut on_stack = vec![false; n];
    let mut indices = vec![None::<usize>; n];
    let mut lowlinks = vec![0usize; n];

    #[allow(clippy::too_many_arguments)]
    fn strongconnect(
        v: usize,
        adj: &[HashSet<usize>],
        index: &mut usize,
        stack: &mut Vec<usize>,
        on_stack: &mut Vec<bool>,
        indices: &mut Vec<Option<usize>>,
        lowlinks: &mut Vec<usize>,
        in_cycle: &mut HashSet<usize>,
    ) {
        indices[v] = Some(*index);
        lowlinks[v] = *index;
        *index += 1;
        stack.push(v);
        on_stack[v] = true;

        for &w in &adj[v] {
            if indices[w].is_none() {
                strongconnect(v.max(w), adj, index, stack, on_stack, indices, lowlinks, in_cycle);
            }
            if w != v && on_stack[w] {
                lowlinks[v] = lowlinks[v].min(indices[w].unwrap());
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
            if component.len() > 1 || adj[v].contains(&v) {
                for &node in &component {
                    in_cycle.insert(node);
                }
            }
        }
    }

    for v in 0..n {
        if indices[v].is_none() {
            strongconnect(
                v, adj, &mut index, &mut stack, &mut on_stack,
                &mut indices, &mut lowlinks, &mut in_cycle,
            );
        }
    }
    in_cycle
}

pub fn composite_health_score(edges: &[(String, String)]) -> HealthReport {
    if edges.is_empty() {
        return HealthReport {
            scores: Vec::new(),
            avg_connectivity: 0.0,
            avg_reachability: 0.0,
            avg_centrality: 0.0,
            avg_cycle_risk: 0.0,
            avg_overall: 0.0,
            node_count: 0,
            edge_count: 0,
        };
    }

    let (node_vec, _node_idx, out_adj, in_adj) = build_graph(edges);
    let n = node_vec.len();
    if n == 0 {
        return HealthReport {
            scores: Vec::new(),
            avg_connectivity: 0.0,
            avg_reachability: 0.0,
            avg_centrality: 0.0,
            avg_cycle_risk: 0.0,
            avg_overall: 0.0,
            node_count: 0,
            edge_count: 0,
        };
    }

    let fwd_reach = compute_reachability(n, &out_adj);
    let bwd_reach = compute_reachability(n, &in_adj);
    let cycle_nodes = find_cycles(n, &out_adj);

    let total_possible = (n - 1).max(1) as f64;
    let total_degree: f64 = out_adj.iter().map(|s| s.len()).sum::<usize>() as f64;
    let avg_degree = total_degree / n as f64;

    let mut scores = Vec::with_capacity(n);
    for (i, name) in node_vec.iter().enumerate() {
        let out_degree = out_adj[i].len() as f64;
        let in_degree = in_adj[i].len() as f64;
        let connectivity = (out_degree + in_degree) / (2.0 * avg_degree.max(1.0)).min(total_degree.max(1.0));
        let connectivity = connectivity.min(1.0);

        let reach_fwd = (fwd_reach[i] as f64 - 1.0) / total_possible;
        let reach_bwd = (bwd_reach[i] as f64 - 1.0) / total_possible;
        let reachability = (reach_fwd + reach_bwd) / 2.0;

        let centrality = if n > 1 {
            (fwd_reach[i] + bwd_reach[i]) as f64 / (2.0 * n as f64)
        } else {
            1.0
        };

        let cycle_risk = if cycle_nodes.contains(&i) { 1.0 } else { 0.0 };

        let overall = connectivity * 0.25 + reachability * 0.25 + centrality * 0.25 + (1.0 - cycle_risk) * 0.25;

        scores.push(HealthScore {
            name: name.clone(),
            overall,
            connectivity,
            reachability,
            centrality,
            cycle_risk,
        });
    }

    let avg_connectivity = scores.iter().map(|s| s.connectivity).sum::<f64>() / n as f64;
    let avg_reachability = scores.iter().map(|s| s.reachability).sum::<f64>() / n as f64;
    let avg_centrality = scores.iter().map(|s| s.centrality).sum::<f64>() / n as f64;
    let avg_cycle_risk = scores.iter().map(|s| s.cycle_risk).sum::<f64>() / n as f64;
    let avg_overall = scores.iter().map(|s| s.overall).sum::<f64>() / n as f64;

    scores.sort_by(|a, b| b.overall.partial_cmp(&a.overall).unwrap_or(std::cmp::Ordering::Equal));

    HealthReport {
        scores,
        avg_connectivity,
        avg_reachability,
        avg_centrality,
        avg_cycle_risk,
        avg_overall,
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
        let report = composite_health_score(&[]);
        assert_eq!(report.node_count, 0);
        assert_eq!(report.scores.len(), 0);
    }

    #[test]
    fn single_edge() {
        let edges = vec![e("a", "b")];
        let report = composite_health_score(&edges);
        assert_eq!(report.node_count, 2);
        assert_eq!(report.scores.len(), 2);
    }

    #[test]
    fn hub_node_scores_higher() {
        let edges = vec![
            e("hub", "a"),
            e("hub", "b"),
            e("hub", "c"),
            e("a", "b"),
        ];
        let report = composite_health_score(&edges);
        let hub = report.scores.iter().find(|s| s.name == "hub").unwrap();
        let a = report.scores.iter().find(|s| s.name == "a").unwrap();
        assert!(hub.overall > a.overall, "hub should score higher than leaf");
    }

    #[test]
    fn cycle_free_zero_cycle_risk() {
        let edges = vec![e("a", "b"), e("b", "c")];
        let report = composite_health_score(&edges);
        for score in &report.scores {
            assert_eq!(score.cycle_risk, 0.0, "{} should have no cycle risk", score.name);
        }
    }

    #[test]
    fn cycle_increases_cycle_risk() {
        let edges = vec![e("a", "b"), e("b", "a")];
        let report = composite_health_score(&edges);
        for score in &report.scores {
            assert_eq!(score.cycle_risk, 1.0, "{} should have cycle risk", score.name);
        }
    }

    #[test]
    fn overall_scores_bounded() {
        let edges = vec![
            e("a", "b"),
            e("b", "c"),
            e("c", "d"),
            e("d", "a"),
            e("a", "c"),
        ];
        let report = composite_health_score(&edges);
        for score in &report.scores {
            assert!(score.overall >= 0.0 && score.overall <= 1.0, "{} overall={}", score.name, score.overall);
            assert!(score.connectivity >= 0.0 && score.connectivity <= 1.0);
            assert!(score.reachability >= 0.0 && score.reachability <= 1.0);
            assert!(score.centrality >= 0.0 && score.centrality <= 1.0);
        }
    }

    #[test]
    fn sorted_by_overall_descending() {
        let edges = vec![e("a", "b"), e("a", "c"), e("b", "c")];
        let report = composite_health_score(&edges);
        for i in 1..report.scores.len() {
            assert!(report.scores[i - 1].overall >= report.scores[i].overall);
        }
    }
}
