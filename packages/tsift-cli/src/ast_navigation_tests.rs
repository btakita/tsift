//! `link_ast_navigation_edges` correctness + perf-gate coverage (#graphrefreshperf).
//!
//! The trigger was a root `graph-db refresh` over haiven-sdk that never
//! finished: C++17 `namespace haiven::wire { ... }` is indexed as two namespace
//! spans over identical bytes, each became the other's AST parent, and the
//! enclosing-module walk spun forever on that two-cycle. Parent resolution was
//! also quadratic per file. These tests pin both: the fast parent resolver must
//! match the quadratic reference exactly, and a synthetic corpus shaped like the
//! trigger must link inside the perf-gate budget.

use super::*;
use std::sync::mpsc;
use std::time::{Duration, Instant};
use tsift_quality::perf_gate::{
    AST_NAVIGATION_LINK_BUDGET_MICROS, PreparationHotspotVerdict, evaluate_preparation_hotspot,
};

fn span_entry(
    file: &str,
    handle: &str,
    name: &str,
    kind: &str,
    start_byte: usize,
    end_byte: usize,
) -> TraversalAstSpanIndexEntry {
    TraversalAstSpanIndexEntry {
        handle: handle.to_string(),
        symbol_handle: String::new(),
        file_handle: Some(format!("gfile-{file}")),
        file: file.to_string(),
        name: name.to_string(),
        kind: kind.to_string(),
        language: "cpp".to_string(),
        node_kind: format!("{kind}_definition"),
        start_byte,
        end_byte,
        parent_module: None,
        markdown: None,
    }
}

fn graph_with_nodes(entries: &[TraversalAstSpanIndexEntry]) -> TraversalGraphBuild {
    let mut graph = TraversalGraphBuild::default();
    let mut handles = entries
        .iter()
        .flat_map(|entry| [Some(entry.handle.clone()), entry.file_handle.clone()])
        .flatten()
        .collect::<Vec<_>>();
    handles.sort();
    handles.dedup();
    for handle in handles {
        graph.add_node(TraversalNode {
            handle: handle.clone(),
            kind: "ast_span".to_string(),
            label: handle,
            ref_id: None,
            path: None,
            line: None,
            detail: None,
            properties: BTreeMap::new(),
            expand: String::new(),
        });
    }
    graph
}

/// Deterministic LCG so the oracle comparison is reproducible without a dep.
struct Lcg(u64);

impl Lcg {
    fn next(&mut self, bound: usize) -> usize {
        self.0 = self
            .0
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        ((self.0 >> 33) as usize) % bound.max(1)
    }
}

/// Laminar spans (a random tree), with identical-span duplicates, duplicate
/// handles, zero-length spans, and optionally crossing spans mixed in.
fn random_file_entries(seed: u64, crossing: bool) -> Vec<TraversalAstSpanIndexEntry> {
    let mut rng = Lcg(seed);
    let mut entries = Vec::new();
    let mut ranges = vec![(0usize, 4_000usize)];
    let kinds = ["namespace", "class", "function", "module", "field"];
    let mut counter = 0usize;
    while let Some((start, end)) = ranges.pop() {
        if entries.len() > 160 {
            break;
        }
        counter += 1;
        let kind = kinds[rng.next(kinds.len())];
        let handle = if rng.next(12) == 0 && counter > 1 {
            // Duplicate handle reused from an earlier span.
            format!("span-{}", rng.next(counter - 1))
        } else {
            format!("span-{counter}")
        };
        entries.push(span_entry(
            "f.cpp",
            &handle,
            &format!("n{}", rng.next(5)),
            kind,
            start,
            end,
        ));
        if rng.next(5) == 0 {
            // Identical-span sibling (the `namespace a::b` shape).
            counter += 1;
            entries.push(span_entry(
                "f.cpp",
                &format!("span-{counter}"),
                &format!("n{}", rng.next(5)),
                kinds[rng.next(kinds.len())],
                start,
                end,
            ));
        }
        let width = end - start;
        if width >= 4 {
            let children = 1 + rng.next(4);
            let step = width / (children + 1);
            for child in 0..children {
                let child_start = start + child * step + rng.next(step.max(1));
                let child_end = (child_start + 1 + rng.next(step.max(1))).min(end);
                if child_start < child_end {
                    ranges.push((child_start, child_end));
                }
            }
        } else if rng.next(3) == 0 {
            ranges.push((start, start));
        }
        if crossing && rng.next(9) == 0 && width >= 4 {
            counter += 1;
            entries.push(span_entry(
                "f.cpp",
                &format!("span-{counter}"),
                "crossing",
                "field",
                start + width / 2,
                end + 7,
            ));
        }
    }
    // Shuffle file order deterministically: tie-breaks depend on it.
    for index in (1..entries.len()).rev() {
        entries.swap(index, rng.next(index + 1));
    }
    entries
}

#[test]
fn fast_ast_parent_resolution_matches_quadratic_reference() {
    for seed in 0..300u64 {
        for crossing in [false, true] {
            let entries = random_file_entries(seed, crossing);
            let refs = entries.iter().collect::<Vec<_>>();
            assert_eq!(
                traversal_ast_parent_indices(&refs),
                traversal_ast_parent_indices_quadratic(&refs),
                "seed {seed} crossing {crossing}"
            );
        }
    }
}

#[test]
fn nested_namespace_identical_spans_do_not_spin_enclosing_module_walk() {
    // `namespace haiven::wire { class ErrorCode { ... }; }` — two namespace spans
    // over the same bytes, neither a module.
    let entries = vec![
        span_entry("e.h", "span-haiven", "haiven", "namespace", 10, 500),
        span_entry("e.h", "span-wire", "wire", "namespace", 10, 500),
        span_entry("e.h", "span-class", "ErrorCode", "class", 40, 480),
        span_entry("e.h", "span-method", "value", "function", 60, 90),
    ];
    let mut graph = graph_with_nodes(&entries);
    let (sender, receiver) = mpsc::channel();
    std::thread::spawn(move || {
        link_ast_navigation_edges(&mut graph, &entries);
        let _ = sender.send(graph);
    });
    let graph = receiver
        .recv_timeout(Duration::from_secs(30))
        .expect("link_ast_navigation_edges must terminate on an identical-span parent cycle");
    let has = |from: &str, to: &str, relation: &str| {
        graph
            .edges
            .iter()
            .any(|edge| edge.from == from && edge.to == to && edge.relation == relation)
    };
    // Identical spans stay each other's parent (pre-fix semantics preserved).
    assert!(has("span-haiven", "span-wire", "parent"));
    assert!(has("span-wire", "span-haiven", "parent"));
    assert!(has("span-class", "span-haiven", "parent"));
    assert!(has("span-method", "span-class", "parent"));
    assert!(
        !graph
            .edges
            .iter()
            .any(|edge| edge.relation == "enclosing_module")
    );
}

/// Synthetic corpus shaped like the haiven-sdk trigger: generated C++ headers
/// with a nested-namespace pair wrapping one class, plus one large generated
/// header whose class has thousands of members (stresses per-file parent
/// resolution, which was quadratic).
fn haiven_shaped_corpus() -> Vec<TraversalAstSpanIndexEntry> {
    let mut entries = Vec::new();
    let mut header = |file: &str, members: usize| {
        let end = 100 + members * 40 + 100;
        entries.push(span_entry(
            file,
            &format!("{file}#ns-a"),
            "haiven",
            "namespace",
            10,
            end,
        ));
        entries.push(span_entry(
            file,
            &format!("{file}#ns-b"),
            "wire",
            "namespace",
            10,
            end,
        ));
        entries.push(span_entry(
            file,
            &format!("{file}#class"),
            "Code",
            "class",
            50,
            end - 20,
        ));
        for member in 0..members {
            let start = 100 + member * 40;
            entries.push(span_entry(
                file,
                &format!("{file}#m{member}"),
                &format!("kMember{member}"),
                "field",
                start,
                start + 30,
            ));
        }
    };
    header("Vocabulary/ErrorCode.h", 12_000);
    for file in 0..200 {
        header(&format!("Vocabulary/Generated{file}.h"), 30);
    }
    entries
}

#[test]
fn perf_gate_link_ast_navigation_edges_on_haiven_shaped_corpus() {
    let entries = haiven_shaped_corpus();
    let template = graph_with_nodes(&entries);
    let mut samples = Vec::new();
    for _ in 0..3 {
        let mut graph = template.clone();
        let run_entries = entries.clone();
        let (sender, receiver) = mpsc::channel();
        std::thread::spawn(move || {
            let started = Instant::now();
            link_ast_navigation_edges(&mut graph, &run_entries);
            let _ = sender.send((started.elapsed().as_micros(), graph.edges.len()));
        });
        let (micros, edges) = receiver
            .recv_timeout(Duration::from_micros(
                (AST_NAVIGATION_LINK_BUDGET_MICROS * 10) as u64,
            ))
            .expect("link_ast_navigation_edges did not finish within 10x the perf-gate budget");
        assert!(
            edges > entries.len() * 3,
            "expected navigation edges, got {edges}"
        );
        eprintln!("link_ast_navigation_edges sample: {micros}µs, {edges} edges");
        samples.push(micros);
    }
    let report = evaluate_preparation_hotspot(
        "graph_refresh.link_ast_navigation_edges",
        &samples,
        AST_NAVIGATION_LINK_BUDGET_MICROS,
    );
    assert_eq!(
        report.verdict,
        PreparationHotspotVerdict::Within,
        "{:?}",
        report.diagnostics
    );
}
