use tsift_quality::token_gate;

use token_gate::{
    MIN_TOKEN_GATE_SAMPLES, TokenGateDecision, TokenSurfaceVerdict, evaluate_token_gate,
    evaluate_token_regression, parse_token_history, TOKEN_GATE_SURFACES,
};

#[allow(clippy::too_many_arguments)]
fn synth_token_sample(
    id: &str,
    surface: &str,
    prompt_tokens: f64,
    envelope_bytes: f64,
    runtime_micros: f64,
    cache_hit_rate: f64,
    raw_read_avoidance: f64,
    useful_hit_density: f64,
) -> serde_json::Value {
    let mut metrics = serde_json::Map::new();
    metrics.insert("prompt_tokens".into(), serde_json::Value::from(prompt_tokens));
    metrics.insert("envelope_bytes".into(), serde_json::Value::from(envelope_bytes));
    metrics.insert("runtime_micros".into(), serde_json::Value::from(runtime_micros));
    metrics.insert(
        "cache_hit_rate_percent".into(),
        serde_json::Value::from(cache_hit_rate),
    );
    metrics.insert(
        "raw_read_avoidance".into(),
        serde_json::Value::from(raw_read_avoidance),
    );
    metrics.insert(
        "useful_hit_density".into(),
        serde_json::Value::from(useful_hit_density),
    );
    let mut entry = serde_json::Map::new();
    entry.insert(
        "label".into(),
        serde_json::Value::from(format!("synth {surface} sample")),
    );
    entry.insert("id".into(), serde_json::Value::from(id.to_string()));
    entry.insert(
        "timestamp".into(),
        serde_json::Value::from("2026-06-02T00:00:00Z"),
    );
    entry.insert(
        "surface".into(),
        serde_json::Value::from(surface.to_string()),
    );
    entry.insert("metrics".into(), serde_json::Value::Object(metrics));
    serde_json::Value::Object(entry)
}

fn build_token_history(samples: Vec<serde_json::Value>) -> String {
    let mut root = serde_json::Map::new();
    root.insert("entries".into(), serde_json::Value::Array(samples));
    serde_json::Value::Object(root).to_string()
}

fn full_token_history_three_samples_each() -> Vec<token_gate::TokenGateSample> {
    let mut entries = Vec::new();
    for surface in TOKEN_GATE_SURFACES {
        for i in 1..=3 {
            entries.push(synth_token_sample(
                &format!("synth-{surface}-2026-06-02-sample-{i}"),
                surface,
                500.0,
                2048.0,
                150_000.0,
                85.0,
                12.0,
                0.72,
            ));
        }
    }
    let raw = build_token_history(entries);
    parse_token_history(&raw).expect("synthetic history parses")
}

#[test]
fn token_gate_passes_when_all_surfaces_have_samples_with_signal() {
    let history = full_token_history_three_samples_each();
    let report = evaluate_token_gate(&history, 10.0);
    assert_eq!(report.decision, TokenGateDecision::Pass, "{report:?}");
    assert_eq!(report.surface_evaluations.len(), TOKEN_GATE_SURFACES.len());
    assert!(report
        .surface_evaluations
        .iter()
        .all(|s| s.verdict == TokenSurfaceVerdict::Pass));
}

#[test]
fn token_gate_blocks_when_a_surface_is_missing() {
    let mut entries = Vec::new();
    for surface in &TOKEN_GATE_SURFACES[..4] {
        for i in 1..=3 {
            entries.push(synth_token_sample(
                &format!("synth-{surface}-2026-06-02-sample-{i}"),
                surface,
                500.0,
                2048.0,
                150_000.0,
                85.0,
                12.0,
                0.72,
            ));
        }
    }
    let raw = build_token_history(entries);
    let history = parse_token_history(&raw).unwrap();
    let report = evaluate_token_gate(&history, 10.0);
    assert_eq!(report.decision, TokenGateDecision::Block);
    let missing = report
        .surface_evaluations
        .iter()
        .filter(|s| s.verdict == TokenSurfaceVerdict::Missing)
        .count();
    assert_eq!(missing, 1);
}

#[test]
fn token_gate_blocks_on_insufficient_samples() {
    let mut entries = Vec::new();
    for surface in TOKEN_GATE_SURFACES {
        for i in 1..=2 {
            entries.push(synth_token_sample(
                &format!("synth-{surface}-2026-06-02-sample-{i}"),
                surface,
                500.0,
                2048.0,
                150_000.0,
                85.0,
                12.0,
                0.72,
            ));
        }
    }
    let raw = build_token_history(entries);
    let history = parse_token_history(&raw).unwrap();
    let report = evaluate_token_gate(&history, 10.0);
    assert_eq!(report.decision, TokenGateDecision::Block);
    assert!(report
        .surface_evaluations
        .iter()
        .all(|s| s.verdict == TokenSurfaceVerdict::InsufficientSamples));
}

#[test]
fn token_regression_passes_when_candidate_matches_baseline() {
    let history = full_token_history_three_samples_each();
    let report = evaluate_token_regression(&history, &history, 10.0);
    assert_eq!(report.decision, TokenGateDecision::Pass, "{report:?}");
}

#[test]
fn token_regression_blocks_on_lower_is_better_regression() {
    let mut baseline_entries = Vec::new();
    let mut candidate_entries = Vec::new();
    for surface in TOKEN_GATE_SURFACES {
        for i in 1..=3 {
            baseline_entries.push(synth_token_sample(
                &format!("base-{surface}-sample-{i}"),
                surface,
                500.0,
                2048.0,
                150_000.0,
                85.0,
                12.0,
                0.72,
            ));
            candidate_entries.push(synth_token_sample(
                &format!("cand-{surface}-sample-{i}"),
                surface,
                5000.0,
                2048.0,
                150_000.0,
                85.0,
                12.0,
                0.72,
            ));
        }
    }
    let baseline =
        parse_token_history(&build_token_history(baseline_entries)).expect("baseline parses");
    let candidate =
        parse_token_history(&build_token_history(candidate_entries)).expect("candidate parses");
    let report = evaluate_token_regression(&baseline, &candidate, 10.0);
    assert_eq!(report.decision, TokenGateDecision::Block);
}

#[test]
fn token_regression_blocks_on_higher_is_better_regression() {
    let mut baseline_entries = Vec::new();
    let mut candidate_entries = Vec::new();
    for surface in TOKEN_GATE_SURFACES {
        for i in 1..=3 {
            baseline_entries.push(synth_token_sample(
                &format!("base-{surface}-sample-{i}"),
                surface,
                500.0,
                2048.0,
                150_000.0,
                85.0,
                12.0,
                0.72,
            ));
            candidate_entries.push(synth_token_sample(
                &format!("cand-{surface}-sample-{i}"),
                surface,
                500.0,
                2048.0,
                150_000.0,
                10.0,
                12.0,
                0.72,
            ));
        }
    }
    let baseline =
        parse_token_history(&build_token_history(baseline_entries)).expect("baseline parses");
    let candidate =
        parse_token_history(&build_token_history(candidate_entries)).expect("candidate parses");
    let report = evaluate_token_regression(&baseline, &candidate, 10.0);
    assert_eq!(report.decision, TokenGateDecision::Block);
}

#[test]
fn parse_token_history_rejects_missing_entries_field() {
    let raw = r#"{"runs": []}"#;
    assert!(parse_token_history(raw).is_err());
}

#[test]
fn parse_token_history_rejects_missing_surface() {
    let raw = r#"{"entries": [{"label": "test", "id": "t1", "metrics": {}}]}"#;
    assert!(parse_token_history(raw).is_err());
}

#[test]
fn min_token_gate_samples_is_three() {
    assert_eq!(MIN_TOKEN_GATE_SAMPLES, 3);
}
