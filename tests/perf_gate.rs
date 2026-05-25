//! Integration tests for the graph DB performance release gate.
//!
//! The decision function lives in `src/perf_gate.rs` and is included here
//! through `#[path]` so the test exercises the same code the binary loads.
//! Tests are read-only against `fixtures/graph-db-performance-history.json`;
//! sibling agents are appending samples concurrently.

#[path = "../src/perf_gate.rs"]
mod perf_gate;

use perf_gate::{
    CONTEXT_PACK_DIFF_BUDGET_MICROS, GateDecision, GateSample, GATE_WORKLOAD_PREFIXES,
    MIN_HOTSPOT_SAMPLES, MIN_SAMPLES_PER_WORKLOAD, PreparationHotspotVerdict, WorkloadVerdict,
    evaluate_preparation_hotspot, evaluate_promotion, parse_history, workload_display_name,
};
use std::collections::BTreeMap;
use std::fs;
use std::path::Path;

const FIXTURE_REL: &str = "fixtures/graph-db-performance-history.json";

fn load_fixture() -> Vec<GateSample> {
    let manifest_dir = env!("CARGO_MANIFEST_DIR");
    let path = Path::new(manifest_dir).join(FIXTURE_REL);
    let raw = fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("failed to read {}: {e}", path.display()));
    parse_history(&raw).unwrap_or_else(|e| panic!("parse_history failed: {e}"))
}

#[test]
fn fixture_entries_parse_with_label_id_and_metrics() {
    let samples = load_fixture();
    assert!(!samples.is_empty(), "fixture should contain at least one run");
    for sample in &samples {
        assert!(
            !sample.label.is_empty(),
            "sample id {} has empty label",
            sample.id
        );
        assert!(!sample.id.is_empty(), "sample missing id");
        assert!(
            !sample.metrics.is_empty(),
            "sample {} has empty metrics map",
            sample.id
        );
        assert!(
            !sample.workload_prefixes.is_empty(),
            "sample {} produced no workload prefixes; expected one of {:?}",
            sample.id,
            GATE_WORKLOAD_PREFIXES
        );
        for prefix in &sample.workload_prefixes {
            assert!(
                GATE_WORKLOAD_PREFIXES.contains(&prefix.as_str()),
                "sample {} produced unknown workload prefix {}",
                sample.id,
                prefix
            );
        }
    }
}

#[test]
fn fixture_sample_ids_encode_sample_index() {
    let samples = load_fixture();
    // Every fixture run that carries the canonical `sample-N` suffix in its id
    // must round-trip through parse_sample_index.
    let mut sample_index_count = 0usize;
    for sample in &samples {
        if sample.id.contains("sample-") {
            assert!(
                sample.sample_index.is_some(),
                "sample id {} should encode an index",
                sample.id
            );
            sample_index_count += 1;
        }
    }
    assert!(
        sample_index_count > 0,
        "expected at least one fixture entry with a sample-N suffix"
    );
}

#[test]
fn fixture_carries_baseline_backend_for_every_workload_it_records() {
    let samples = load_fixture();
    // Every workload present in the fixture must include the `sqlite` baseline
    // backend so the gate has a comparison anchor. Workloads not yet recorded
    // by sibling agents are tolerated; they will be enforced through the
    // missing-workload branch of `evaluate_promotion`.
    let mut workload_backends: BTreeMap<String, std::collections::BTreeSet<String>> =
        BTreeMap::new();
    for sample in &samples {
        for (workload, backends) in &sample.backends_by_workload {
            workload_backends
                .entry(workload.clone())
                .or_default()
                .extend(backends.iter().cloned());
        }
    }
    for (workload, backends) in &workload_backends {
        assert!(
            backends.contains("sqlite"),
            "workload `{}` ({}) in fixture is missing the `sqlite` baseline backend; got {:?}",
            workload,
            workload_display_name(workload),
            backends
        );
    }
}

#[test]
fn gate_blocks_when_fewer_than_three_samples_for_any_workload() {
    // Build a history with only 2 samples for `synthetic_deep_chain`; the
    // remaining three workloads each get the required 3.
    let history = build_synthetic_history(&[
        ("real", 3, 1000.0, 100.0),
        ("full_projection", 3, 1000.0, 100.0),
        ("synthetic_high_degree", 3, 1000.0, 100.0),
        ("synthetic_deep_chain", 2, 1000.0, 100.0),
    ]);
    let report = evaluate_promotion(&history, "falkordb", 0.0);
    assert_eq!(report.decision, GateDecision::Block);
    let dc = report
        .workload_evaluations
        .iter()
        .find(|w| w.workload == "synthetic_deep_chain")
        .expect("deep-chain workload present in report");
    assert_eq!(
        dc.verdict,
        WorkloadVerdict::InsufficientSamples,
        "expected insufficient-sample verdict for synthetic_deep_chain; got {:?}",
        dc
    );
    assert_eq!(dc.sample_count, 2);
    assert_eq!(MIN_SAMPLES_PER_WORKLOAD, 3);
}

#[test]
fn gate_blocks_candidate_that_does_not_beat_sqlite() {
    let history = build_synthetic_history(&[
        ("real", 3, 1000.0, 1500.0),
        ("full_projection", 3, 1000.0, 1500.0),
        ("synthetic_high_degree", 3, 1000.0, 1500.0),
        ("synthetic_deep_chain", 3, 1000.0, 1500.0),
    ]);
    let report = evaluate_promotion(&history, "falkordb", 0.0);
    assert_eq!(report.decision, GateDecision::Block);
}

#[test]
fn gate_promotes_candidate_that_beats_sqlite_on_every_workload() {
    let history = build_synthetic_history(&[
        ("real", 3, 1000.0, 100.0),
        ("full_projection", 3, 1000.0, 100.0),
        ("synthetic_high_degree", 3, 1000.0, 100.0),
        ("synthetic_deep_chain", 3, 1000.0, 100.0),
    ]);
    let report = evaluate_promotion(&history, "falkordb", 0.05);
    assert_eq!(
        report.decision,
        GateDecision::Promote,
        "diagnostics={:?}",
        report.diagnostics
    );
}

// ---------------------------------------------------------------------------
// #gdbprephot: preparation hotspot regression gate (context_pack_diff)
// ---------------------------------------------------------------------------

#[test]
fn preparation_hotspot_gate_passes_when_post_fix_samples_under_budget() {
    // Synthetic post-fix samples around 80ms median; budget is 250ms.
    let report = evaluate_preparation_hotspot(
        "conflict_matrix_preparation.context_pack_diff",
        &[70_000, 80_000, 95_000],
        CONTEXT_PACK_DIFF_BUDGET_MICROS,
    );
    assert_eq!(report.verdict, PreparationHotspotVerdict::Within);
    assert_eq!(report.observed_median_micros, Some(80_000));
    assert_eq!(report.min_samples, MIN_HOTSPOT_SAMPLES);
}

#[test]
fn preparation_hotspot_gate_fails_closed_on_pre_fix_baseline() {
    // The actual measured pre-fix medians from the gdbprephot baseline runs
    // (default 3 samples). This locks the regression-detection direction:
    // if the parse budget is ever removed, samples climb back to ~445ms and
    // the gate must trip.
    let report = evaluate_preparation_hotspot(
        "conflict_matrix_preparation.context_pack_diff",
        &[436_658, 445_507, 462_138],
        CONTEXT_PACK_DIFF_BUDGET_MICROS,
    );
    assert_eq!(report.verdict, PreparationHotspotVerdict::Regressed);
    assert_eq!(report.observed_median_micros, Some(445_507));
    assert!(report.diagnostics[0].contains("REGRESSED"));
}

#[test]
fn preparation_hotspot_gate_refuses_to_decide_below_three_samples() {
    let report = evaluate_preparation_hotspot(
        "conflict_matrix_preparation.context_pack_diff",
        &[1_000, 2_000],
        CONTEXT_PACK_DIFF_BUDGET_MICROS,
    );
    assert_eq!(
        report.verdict,
        PreparationHotspotVerdict::InsufficientSamples
    );
    assert_eq!(report.observed_median_micros, None);
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn build_synthetic_history(
    workloads: &[(&str, usize, f64, f64)],
) -> Vec<GateSample> {
    let mut runs = Vec::new();
    for (prefix, n, sqlite_us, cand_us) in workloads {
        for i in 1..=*n {
            let mut metrics = serde_json::Map::new();
            metrics.insert(
                format!("{prefix}.sqlite.refresh.duration_micros"),
                (*sqlite_us).into(),
            );
            metrics.insert(
                format!("{prefix}.sqlite.total_duration_micros"),
                (sqlite_us * 2.0).into(),
            );
            metrics.insert(
                format!("{prefix}.falkordb.refresh.duration_micros"),
                (*cand_us).into(),
            );
            metrics.insert(
                format!("{prefix}.falkordb.total_duration_micros"),
                (cand_us * 2.0).into(),
            );
            let mut run = serde_json::Map::new();
            run.insert(
                "label".into(),
                format!("graph-db backend-eval {prefix} synth sample {i}").into(),
            );
            run.insert(
                "id".into(),
                format!("synth-{prefix}-2026-05-24-sample-{i}").into(),
            );
            run.insert("metrics".into(), serde_json::Value::Object(metrics));
            runs.push(serde_json::Value::Object(run));
        }
    }
    let mut root = serde_json::Map::new();
    root.insert("runs".into(), serde_json::Value::Array(runs));
    let raw = serde_json::Value::Object(root).to_string();
    parse_history(&raw).expect("synthetic history parses")
}
