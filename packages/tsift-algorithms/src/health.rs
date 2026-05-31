use crate::graph_builder::build_graph;
use serde::{Deserialize, Serialize};
use std::collections::HashSet;

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

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TerseHealthReport {
    pub top_scores: Vec<TerseHealthScore>,
    pub bottom_scores: Vec<TerseHealthScore>,
    pub avg_overall: f64,
    pub avg_cycle_risk: f64,
    pub node_count: usize,
    pub edge_count: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TerseHealthScore {
    pub name: String,
    pub overall: f64,
}

pub fn terse_health_report(edges: &[(String, String)], n: usize) -> TerseHealthReport {
    if edges.is_empty() {
        return TerseHealthReport {
            top_scores: Vec::new(),
            bottom_scores: Vec::new(),
            avg_overall: 0.0,
            avg_cycle_risk: 0.0,
            node_count: 0,
            edge_count: 0,
        };
    }

    let graph = build_graph(edges);
    let node_vec = &graph.node_vec;
    let out_adj = &graph.out_adj;
    let in_adj = &graph.in_adj;
    let node_count = graph.node_count();
    if node_count == 0 {
        return TerseHealthReport {
            top_scores: Vec::new(),
            bottom_scores: Vec::new(),
            avg_overall: 0.0,
            avg_cycle_risk: 0.0,
            node_count: 0,
            edge_count: 0,
        };
    }

    let fwd_sccs = compute_sccs(node_count, &out_adj);
    let bwd_sccs = compute_sccs(node_count, &in_adj);
    let cycle_nodes = find_cycles_from_sccs(&out_adj, &fwd_sccs);

    let fwd_reach = compute_reachability_with_sccs(node_count, &out_adj, &fwd_sccs);
    let bwd_reach = compute_reachability_with_sccs(node_count, &in_adj, &bwd_sccs);

    let total_possible = (node_count - 1).max(1) as f64;
    let total_degree: f64 = out_adj.iter().map(|s| s.len()).sum::<usize>() as f64;
    let avg_degree = total_degree / node_count as f64;

    let mut raw_scores: Vec<TerseHealthScore> = Vec::with_capacity(node_count);
    let mut sum_overall: f64 = 0.0;
    let mut sum_cycle_risk: f64 = 0.0;

    for (i, name) in node_vec.iter().enumerate() {
        let out_degree = out_adj[i].len() as f64;
        let in_degree = in_adj[i].len() as f64;
        let connectivity =
            (out_degree + in_degree) / (2.0 * avg_degree.max(1.0)).min(total_degree.max(1.0));
        let connectivity = connectivity.min(1.0);

        let reach_fwd = (fwd_reach[i] as f64 - 1.0) / total_possible;
        let reach_bwd = (bwd_reach[i] as f64 - 1.0) / total_possible;
        let reachability = (reach_fwd + reach_bwd) / 2.0;

        let centrality = if node_count > 1 {
            (fwd_reach[i] + bwd_reach[i]) as f64 / (2.0 * node_count as f64)
        } else {
            1.0
        };

        let cycle_risk = if cycle_nodes.contains(&i) { 1.0 } else { 0.0 };

        let overall = connectivity * 0.25
            + reachability * 0.25
            + centrality * 0.25
            + (1.0 - cycle_risk) * 0.25;

        sum_overall += overall;
        sum_cycle_risk += cycle_risk;

        raw_scores.push(TerseHealthScore {
            name: name.clone(),
            overall,
        });
    }

    let n = n.min(node_count);

    let (mut top_candidates, mut bottom_candidates) = {
        let mut sorted = raw_scores;
        sorted.sort_by(|a, b| b.overall.partial_cmp(&a.overall).unwrap_or(std::cmp::Ordering::Equal));
        let top: Vec<TerseHealthScore> = sorted.iter().take(n).cloned().collect();
        let bottom: Vec<TerseHealthScore> = sorted.iter().rev().take(n).cloned().collect();
        (top, bottom)
    };

    top_candidates.sort_by(|a, b| b.overall.partial_cmp(&a.overall).unwrap_or(std::cmp::Ordering::Equal));
    bottom_candidates.sort_by(|a, b| a.overall.partial_cmp(&b.overall).unwrap_or(std::cmp::Ordering::Equal));

    TerseHealthReport {
        top_scores: top_candidates,
        bottom_scores: bottom_candidates,
        avg_overall: sum_overall / node_count as f64,
        avg_cycle_risk: sum_cycle_risk / node_count as f64,
        node_count,
        edge_count: edges.len(),
    }
}

fn compute_reachability_with_sccs(
    n: usize,
    adj: &[HashSet<usize>],
    sccs: &[Vec<usize>],
) -> Vec<usize> {
    let mut comp_of = vec![0usize; n];
    for (comp_id, members) in sccs.iter().enumerate() {
        for &node in members {
            comp_of[node] = comp_id;
        }
    }
    let num_comps = sccs.len();
    let mut comp_adj: Vec<HashSet<usize>> = vec![HashSet::new(); num_comps];
    for u in 0..n {
        for &v in &adj[u] {
            let cu = comp_of[u];
            let cv = comp_of[v];
            if cu != cv {
                comp_adj[cu].insert(cv);
            }
        }
    }
    let mut comp_reach = vec![0usize; num_comps];
    for c in 0..num_comps {
        let mut visited = vec![false; num_comps];
        visited[c] = true;
        let mut count = 0usize;
        let mut queue = std::collections::VecDeque::from([c]);
        while let Some(cur) = queue.pop_front() {
            count += sccs[cur].len();
            for &next in &comp_adj[cur] {
                if !visited[next] {
                    visited[next] = true;
                    queue.push_back(next);
                }
            }
        }
        comp_reach[c] = count;
    }
    let mut reach = vec![0usize; n];
    for i in 0..n {
        reach[i] = comp_reach[comp_of[i]];
    }
    reach
}

fn compute_sccs(n: usize, adj: &[HashSet<usize>]) -> Vec<Vec<usize>> {
    let mut index = 0usize;
    let mut stack = Vec::new();
    let mut on_stack = vec![false; n];
    let mut indices = vec![None::<usize>; n];
    let mut lowlinks = vec![0usize; n];
    let mut components: Vec<Vec<usize>> = Vec::new();

    fn strongconnect(
        v: usize,
        adj: &[HashSet<usize>],
        index: &mut usize,
        stack: &mut Vec<usize>,
        on_stack: &mut Vec<bool>,
        indices: &mut Vec<Option<usize>>,
        lowlinks: &mut Vec<usize>,
        components: &mut Vec<Vec<usize>>,
    ) {
        indices[v] = Some(*index);
        lowlinks[v] = *index;
        *index += 1;
        stack.push(v);
        on_stack[v] = true;

        for &w in &adj[v] {
            if indices[w].is_none() {
                strongconnect(w, adj, index, stack, on_stack, indices, lowlinks, components);
            }
            if on_stack[w] {
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
            components.push(component);
        }
    }

    for v in 0..n {
        if indices[v].is_none() {
            strongconnect(
                v,
                adj,
                &mut index,
                &mut stack,
                &mut on_stack,
                &mut indices,
                &mut lowlinks,
                &mut components,
            );
        }
    }
    components
}

fn find_cycles_from_sccs(adj: &[HashSet<usize>], sccs: &[Vec<usize>]) -> HashSet<usize> {
    let mut in_cycle = HashSet::new();
    for component in sccs {
        if component.len() > 1 {
            for &node in component {
                in_cycle.insert(node);
            }
        } else if component.len() == 1 {
            let v = component[0];
            if adj[v].contains(&v) {
                in_cycle.insert(v);
            }
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

    let graph = build_graph(edges);
    let node_vec = &graph.node_vec;
    let out_adj = &graph.out_adj;
    let in_adj = &graph.in_adj;
    let n = graph.node_count();
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

    let fwd_sccs = compute_sccs(n, &out_adj);
    let bwd_sccs = compute_sccs(n, &in_adj);
    let cycle_nodes = find_cycles_from_sccs(&out_adj, &fwd_sccs);

    let fwd_reach = compute_reachability_with_sccs(n, &out_adj, &fwd_sccs);
    let bwd_reach = compute_reachability_with_sccs(n, &in_adj, &bwd_sccs);

    let total_possible = (n - 1).max(1) as f64;
    let total_degree: f64 = out_adj.iter().map(|s| s.len()).sum::<usize>() as f64;
    let avg_degree = total_degree / n as f64;

    let mut scores = Vec::with_capacity(n);
    for (i, name) in node_vec.iter().enumerate() {
        let out_degree = out_adj[i].len() as f64;
        let in_degree = in_adj[i].len() as f64;
        let connectivity =
            (out_degree + in_degree) / (2.0 * avg_degree.max(1.0)).min(total_degree.max(1.0));
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

        let overall = connectivity * 0.25
            + reachability * 0.25
            + centrality * 0.25
            + (1.0 - cycle_risk) * 0.25;

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

    scores.sort_by(|a, b| {
        b.overall
            .partial_cmp(&a.overall)
            .unwrap_or(std::cmp::Ordering::Equal)
    });

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
        let edges = vec![e("hub", "a"), e("hub", "b"), e("hub", "c"), e("a", "b")];
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
            assert_eq!(
                score.cycle_risk, 0.0,
                "{} should have no cycle risk",
                score.name
            );
        }
    }

    #[test]
    fn cycle_increases_cycle_risk() {
        let edges = vec![e("a", "b"), e("b", "a")];
        let report = composite_health_score(&edges);
        for score in &report.scores {
            assert_eq!(
                score.cycle_risk, 1.0,
                "{} should have cycle risk",
                score.name
            );
        }
    }

    #[test]
    fn dense_cycle_does_not_recurse_on_current_node() {
        let edges = vec![
            e("alpha", "beta"),
            e("alpha", "gamma"),
            e("beta", "alpha"),
            e("beta", "gamma"),
            e("gamma", "alpha"),
            e("gamma", "beta"),
        ];
        let report = composite_health_score(&edges);
        assert_eq!(report.node_count, 3);
        assert_eq!(report.avg_cycle_risk, 1.0);
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
            assert!(
                score.overall >= 0.0 && score.overall <= 1.0,
                "{} overall={}",
                score.name,
                score.overall
            );
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

    #[test]
    fn terse_empty() {
        let report = terse_health_report(&[], 5);
        assert_eq!(report.node_count, 0);
        assert_eq!(report.top_scores.len(), 0);
        assert_eq!(report.bottom_scores.len(), 0);
        assert_eq!(report.avg_overall, 0.0);
    }

    #[test]
    fn terse_matches_composite() {
        let edges = vec![e("a", "b"), e("a", "c"), e("b", "c"), e("c", "d"), e("d", "a")];
        let full = composite_health_score(&edges);
        let terse = terse_health_report(&edges, 2);
        assert_eq!(terse.node_count, full.node_count);
        assert_eq!(terse.edge_count, full.edge_count);
        assert!((terse.avg_overall - full.avg_overall).abs() < 1e-10);
        assert!((terse.avg_cycle_risk - full.avg_cycle_risk).abs() < 1e-10);
        assert_eq!(terse.top_scores.len(), 2);
        assert_eq!(terse.bottom_scores.len(), 2);
        assert_eq!(terse.top_scores[0].name, full.scores[0].name);
        assert!((terse.top_scores[0].overall - full.scores[0].overall).abs() < 1e-10);
    }

    #[test]
    fn terse_top_sorted_descending() {
        let edges = vec![e("hub", "a"), e("hub", "b"), e("hub", "c"), e("a", "b")];
        let terse = terse_health_report(&edges, 2);
        for i in 1..terse.top_scores.len() {
            assert!(terse.top_scores[i - 1].overall >= terse.top_scores[i].overall);
        }
    }

    #[test]
    fn terse_bottom_sorted_ascending() {
        let edges = vec![e("hub", "a"), e("hub", "b"), e("hub", "c"), e("a", "b")];
        let terse = terse_health_report(&edges, 2);
        for i in 1..terse.bottom_scores.len() {
            assert!(terse.bottom_scores[i - 1].overall <= terse.bottom_scores[i].overall);
        }
    }

    #[test]
    fn terse_n_larger_than_nodes() {
        let edges = vec![e("a", "b")];
        let terse = terse_health_report(&edges, 10);
        assert_eq!(terse.top_scores.len(), 2);
        assert_eq!(terse.bottom_scores.len(), 2);
    }

    #[test]
    fn terse_scores_bounded() {
        let edges = vec![
            e("a", "b"),
            e("b", "c"),
            e("c", "d"),
            e("d", "a"),
            e("a", "c"),
        ];
        let terse = terse_health_report(&edges, 2);
        for s in terse.top_scores.iter().chain(terse.bottom_scores.iter()) {
            assert!(s.overall >= 0.0 && s.overall <= 1.0, "{} overall={}", s.name, s.overall);
        }
    }

    #[test]
    fn terse_cycle_avg() {
        let edges = vec![e("a", "b"), e("b", "a")];
        let terse = terse_health_report(&edges, 1);
        assert!((terse.avg_cycle_risk - 1.0).abs() < 1e-10);
    }
}
