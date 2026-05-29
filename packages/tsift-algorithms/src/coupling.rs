use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap, HashSet};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModuleCoupling {
    pub module: String,
    pub fan_out: usize,
    pub fan_in: usize,
    pub instability: f64,
    pub afferent_coupling: usize,
    pub efferent_coupling: usize,
    pub dependents: Vec<String>,
    pub dependencies: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CouplingReport {
    pub modules: Vec<ModuleCoupling>,
    pub avg_fan_out: f64,
    pub avg_fan_in: f64,
    pub max_fan_out: usize,
    pub max_fan_in: usize,
    pub highly_coupled: Vec<String>,
    pub total_modules: usize,
    pub total_edges: usize,
}

pub fn coupling_analysis(
    edges: &[(String, String)],
    module_of: &HashMap<String, String>,
) -> CouplingReport {
    let modules: HashSet<&str> = module_of.values().map(|s| s.as_str()).collect();
    let mut module_list: Vec<&str> = modules.into_iter().collect();
    module_list.sort();

    if module_list.is_empty() || edges.is_empty() {
        return CouplingReport {
            modules: Vec::new(),
            avg_fan_out: 0.0,
            avg_fan_in: 0.0,
            max_fan_out: 0,
            max_fan_in: 0,
            highly_coupled: Vec::new(),
            total_modules: 0,
            total_edges: 0,
        };
    }

    let mut efferent: BTreeMap<String, HashSet<String>> = BTreeMap::new();
    let mut afferent: BTreeMap<String, HashSet<String>> = BTreeMap::new();

    for name in &module_list {
        let m = name.to_string();
        efferent.entry(m.clone()).or_default();
        afferent.entry(m).or_default();
    }

    for (from, to) in edges {
        let from_mod = match module_of.get(from) {
            Some(m) => m.clone(),
            None => continue,
        };
        let to_mod = match module_of.get(to) {
            Some(m) => m.clone(),
            None => continue,
        };
        if from_mod != to_mod {
            efferent.entry(from_mod.clone()).or_default().insert(to_mod.clone());
            afferent.entry(to_mod).or_default().insert(from_mod);
        }
    }

    let mut modules_out = Vec::new();
    let mut max_fan_out = 0usize;
    let mut max_fan_in = 0usize;
    let mut total_fan_out = 0usize;
    let mut total_fan_in = 0usize;

    for name in &module_list {
        let n = name.to_string();
        let deps = efferent.get(&n).cloned().unwrap_or_default();
        let dependents = afferent.get(&n).cloned().unwrap_or_default();
        let fan_out = deps.len();
        let fan_in = dependents.len();
        max_fan_out = max_fan_out.max(fan_out);
        max_fan_in = max_fan_in.max(fan_in);
        total_fan_out += fan_out;
        total_fan_in += fan_in;

        let instability = if fan_in + fan_out > 0 {
            fan_out as f64 / (fan_in + fan_out) as f64
        } else {
            0.0
        };

        let mut dep_list: Vec<String> = deps.into_iter().collect();
        dep_list.sort();
        let mut dep_of: Vec<String> = dependents.into_iter().collect();
        dep_of.sort();

        modules_out.push(ModuleCoupling {
            module: n,
            fan_out,
            fan_in,
            instability,
            afferent_coupling: fan_in,
            efferent_coupling: fan_out,
            dependents: dep_of,
            dependencies: dep_list,
        });
    }

    let num_modules = modules_out.len();
    let avg_fan_out = if num_modules > 0 { total_fan_out as f64 / num_modules as f64 } else { 0.0 };
    let avg_fan_in = if num_modules > 0 { total_fan_in as f64 / num_modules as f64 } else { 0.0 };

    let avg_plus_std = avg_fan_out + (avg_fan_out * 0.5).max(1.0);
    let highly_coupled: Vec<String> = modules_out
        .iter()
        .filter(|m| m.fan_out as f64 > avg_plus_std)
        .map(|m| m.module.clone())
        .collect();

    modules_out.sort_by(|a, b| b.fan_out.cmp(&a.fan_out).then(a.module.cmp(&b.module)));

    CouplingReport {
        modules: modules_out,
        avg_fan_out,
        avg_fan_in,
        max_fan_out,
        max_fan_in,
        highly_coupled,
        total_modules: num_modules,
        total_edges: edges.len(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn e(a: &str, b: &str) -> (String, String) {
        (a.to_string(), b.to_string())
    }

    fn make_module_map(pairs: &[(&str, &str)]) -> HashMap<String, String> {
        pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect()
    }

    #[test]
    fn empty_edges() {
        let report = coupling_analysis(&[], &HashMap::new());
        assert_eq!(report.total_modules, 0);
    }

    #[test]
    fn single_module_no_coupling() {
        let edges = vec![e("a", "b"), e("b", "c")];
        let map = make_module_map(&[("a", "core"), ("b", "core"), ("c", "core")]);
        let report = coupling_analysis(&edges, &map);
        assert_eq!(report.total_modules, 1);
        assert_eq!(report.modules[0].fan_out, 0);
        assert_eq!(report.modules[0].fan_in, 0);
    }

    #[test]
    fn two_modules_coupling() {
        let edges = vec![e("a", "b")];
        let map = make_module_map(&[("a", "mod_a"), ("b", "mod_b")]);
        let report = coupling_analysis(&edges, &map);
        assert_eq!(report.total_modules, 2);
        let mod_a = report.modules.iter().find(|m| m.module == "mod_a").unwrap();
        let mod_b = report.modules.iter().find(|m| m.module == "mod_b").unwrap();
        assert_eq!(mod_a.fan_out, 1);
        assert_eq!(mod_b.fan_in, 1);
        assert!(mod_a.dependencies.contains(&"mod_b".to_string()));
        assert!(mod_b.dependents.contains(&"mod_a".to_string()));
    }

    #[test]
    fn instability_computed() {
        let edges = vec![e("a", "b"), e("a", "c"), e("d", "a")];
        let map = make_module_map(&[
            ("a", "core"),
            ("b", "util"),
            ("c", "util"),
            ("d", "cli"),
        ]);
        let report = coupling_analysis(&edges, &map);
        let core = report.modules.iter().find(|m| m.module == "core").unwrap();
        assert_eq!(core.fan_out, 1); // core -> util
        assert_eq!(core.fan_in, 1); // cli -> core
        let expected = 1.0 / 2.0;
        assert!((core.instability - expected).abs() < 0.001);
    }

    #[test]
    fn highly_coupled_detected() {
        let mut edges = Vec::new();
        let mut map = HashMap::new();
        for i in 0..20 {
            map.insert(format!("hub_{i}"), "hub".to_string());
            map.insert(format!("target_{i}"), format!("target_{i}"));
            edges.push(e(&format!("hub_{i}"), &format!("target_{i}")));
        }
        let report = coupling_analysis(&edges, &map);
        assert!(report.highly_coupled.contains(&"hub".to_string()));
    }

    #[test]
    fn sorted_by_fan_out_descending() {
        let edges = vec![e("a", "b"), e("a", "c"), e("b", "c")];
        let map = make_module_map(&[("a", "mod_a"), ("b", "mod_b"), ("c", "mod_c")]);
        let report = coupling_analysis(&edges, &map);
        for i in 1..report.modules.len() {
            assert!(report.modules[i - 1].fan_out >= report.modules[i].fan_out);
        }
    }

    #[test]
    fn unknown_nodes_skipped() {
        let edges = vec![e("a", "unknown")];
        let map = make_module_map(&[("a", "core")]);
        let report = coupling_analysis(&edges, &map);
        assert_eq!(report.total_modules, 1);
    }
}
