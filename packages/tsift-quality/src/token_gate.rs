//! Cross-surface token/performance gate (#tokegate).
//!
//! Records and gates token efficiency across tsift agent-facing surfaces:
//! `context-pack`, `session-review --next-context`, `graph-db evidence`,
//! `conflict-matrix`, and `dispatch-trace`.
//!
//! For each surface the gate tracks: prompt tokens, envelope bytes,
//! runtime, cache-hit rate, raw-read avoidance, and useful-hit density.
//! A regression on any required metric across any surface blocks the gate.
//!
//! Spec: see specs/graph.md § "Cross-Surface Token Gate".

use anyhow::{Context, Result, bail};
use serde::Serialize;
use serde_json::Value;
use std::collections::BTreeMap;

pub const MIN_TOKEN_GATE_SAMPLES: usize = 3;

pub const TOKEN_GATE_SURFACES: [&str; 5] = [
    "context_pack",
    "session_review_next_context",
    "graph_db_evidence",
    "conflict_matrix",
    "dispatch_trace",
];

pub fn surface_display_name(surface: &str) -> &'static str {
    match surface {
        "context_pack" => "context-pack",
        "session_review_next_context" => "session-review --next-context",
        "graph_db_evidence" => "graph-db evidence",
        "conflict_matrix" => "conflict-matrix",
        "dispatch_trace" => "dispatch-trace",
        _ => "unknown",
    }
}

pub const REQUIRED_TOKEN_METRICS: [&str; 6] = [
    "prompt_tokens",
    "envelope_bytes",
    "runtime_micros",
    "cache_hit_rate_percent",
    "raw_read_avoidance",
    "useful_hit_density",
];

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum TokenMetricDirection {
    LowerIsBetter,
    HigherIsBetter,
}

pub fn metric_direction(metric: &str) -> TokenMetricDirection {
    match metric {
        "prompt_tokens" | "envelope_bytes" | "runtime_micros" => {
            TokenMetricDirection::LowerIsBetter
        }
        "cache_hit_rate_percent" | "raw_read_avoidance" | "useful_hit_density" => {
            TokenMetricDirection::HigherIsBetter
        }
        _ => TokenMetricDirection::LowerIsBetter,
    }
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct TokenGateSample {
    pub label: String,
    pub id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub timestamp: Option<String>,
    pub surface: String,
    pub metrics: BTreeMap<String, f64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum TokenSurfaceVerdict {
    Pass,
    Regressed,
    InsufficientSamples,
    Missing,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct TokenSurfaceMetricEvaluation {
    pub metric: String,
    pub direction: TokenMetricDirection,
    pub baseline_median: Option<f64>,
    pub candidate_median: Option<f64>,
    pub passed: bool,
    pub diagnostic: String,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct TokenSurfaceEvaluation {
    pub surface: String,
    pub display_name: String,
    pub sample_count: usize,
    pub verdict: TokenSurfaceVerdict,
    pub metric_evaluations: Vec<TokenSurfaceMetricEvaluation>,
    pub diagnostics: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum TokenGateDecision {
    Pass,
    Block,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct TokenGateReport {
    pub min_samples: usize,
    pub allowed_regression_percent: f64,
    pub surface_evaluations: Vec<TokenSurfaceEvaluation>,
    pub decision: TokenGateDecision,
    pub diagnostics: Vec<String>,
}

pub fn parse_token_history(raw: &str) -> Result<Vec<TokenGateSample>> {
    let value: Value =
        serde_json::from_str(raw).context("token_gate: failed to parse history JSON")?;
    let entries = match value {
        Value::Object(mut obj) => match obj.remove("entries") {
            Some(Value::Array(arr)) => arr,
            Some(other) => bail!(
                "token_gate: history `entries` field must be an array, got {}",
                value_type_name(&other)
            ),
            None => bail!("token_gate: history JSON object missing `entries` array"),
        },
        Value::Array(arr) => arr,
        other => bail!(
            "token_gate: history root must be object or array, got {}",
            value_type_name(&other)
        ),
    };

    let mut samples = Vec::with_capacity(entries.len());
    for (idx, entry) in entries.into_iter().enumerate() {
        let obj = entry
            .as_object()
            .with_context(|| format!("token_gate: entry #{idx} must be a JSON object"))?;
        let label = obj
            .get("label")
            .and_then(|v| v.as_str())
            .with_context(|| format!("token_gate: entry #{idx} missing string `label`"))?
            .to_string();
        let id = obj
            .get("id")
            .and_then(|v| v.as_str())
            .with_context(|| format!("token_gate: entry #{idx} missing string `id`"))?
            .to_string();
        let timestamp = obj
            .get("timestamp")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());
        let surface = obj
            .get("surface")
            .and_then(|v| v.as_str())
            .with_context(|| format!("token_gate: entry #{idx} missing string `surface`"))?
            .to_string();
        let metrics_value = obj
            .get("metrics")
            .with_context(|| format!("token_gate: entry #{idx} missing `metrics` map"))?;
        let metrics_obj = metrics_value
            .as_object()
            .with_context(|| format!("token_gate: entry #{idx} `metrics` must be an object"))?;
        let mut metrics = BTreeMap::new();
        for (key, val) in metrics_obj {
            if let Some(n) = val.as_f64() {
                metrics.insert(key.clone(), n);
            }
        }

        samples.push(TokenGateSample {
            label,
            id,
            timestamp,
            surface,
            metrics,
        });
    }
    Ok(samples)
}

fn value_type_name(v: &Value) -> &'static str {
    match v {
        Value::Null => "null",
        Value::Bool(_) => "bool",
        Value::Number(_) => "number",
        Value::String(_) => "string",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
    }
}

fn median_f64(values: &[f64]) -> Option<f64> {
    if values.is_empty() {
        return None;
    }
    let mut sorted: Vec<f64> = values.iter().copied().filter(|v| v.is_finite()).collect();
    if sorted.is_empty() {
        return None;
    }
    sorted.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let n = sorted.len();
    if n.is_multiple_of(2) {
        Some((sorted[n / 2 - 1] + sorted[n / 2]) / 2.0)
    } else {
        Some(sorted[n / 2])
    }
}

pub fn evaluate_token_gate(
    history: &[TokenGateSample],
    allowed_regression_percent: f64,
) -> TokenGateReport {
    let mut surface_evaluations = Vec::with_capacity(TOKEN_GATE_SURFACES.len());
    let mut top_diagnostics = Vec::new();
    let mut any_block = false;

    for surface in TOKEN_GATE_SURFACES {
        let display = surface_display_name(surface).to_string();
        let surface_samples: Vec<&TokenGateSample> =
            history.iter().filter(|s| s.surface == surface).collect();
        let sample_count = surface_samples.len();

        if sample_count == 0 {
            surface_evaluations.push(TokenSurfaceEvaluation {
                surface: surface.to_string(),
                display_name: display.clone(),
                sample_count: 0,
                verdict: TokenSurfaceVerdict::Missing,
                metric_evaluations: Vec::new(),
                diagnostics: vec![format!(
                    "surface `{display}` has no samples in history; gate blocks until at least {MIN_TOKEN_GATE_SAMPLES} samples are recorded"
                )],
            });
            any_block = true;
            top_diagnostics.push(format!("`{display}`: missing"));
            continue;
        }

        if sample_count < MIN_TOKEN_GATE_SAMPLES {
            surface_evaluations.push(TokenSurfaceEvaluation {
                surface: surface.to_string(),
                display_name: display.clone(),
                sample_count,
                verdict: TokenSurfaceVerdict::InsufficientSamples,
                metric_evaluations: Vec::new(),
                diagnostics: vec![format!(
                    "surface `{display}` has {sample_count} sample(s); gate requires {MIN_TOKEN_GATE_SAMPLES}"
                )],
            });
            any_block = true;
            top_diagnostics.push(format!(
                "`{display}`: only {sample_count}/{MIN_TOKEN_GATE_SAMPLES} samples"
            ));
            continue;
        }

        let mut metric_evaluations = Vec::with_capacity(REQUIRED_TOKEN_METRICS.len());
        let mut surface_pass = true;

        for metric_name in REQUIRED_TOKEN_METRICS {
            let values: Vec<f64> = surface_samples
                .iter()
                .filter_map(|s| s.metrics.get(metric_name).copied())
                .collect();

            let direction = metric_direction(metric_name);
            let median = if values.len() >= MIN_TOKEN_GATE_SAMPLES {
                median_f64(&values)
            } else {
                None
            };

            let passed = median.is_some_and(|m| match direction {
                TokenMetricDirection::LowerIsBetter => m > 0.0,
                TokenMetricDirection::HigherIsBetter => m > 0.0,
            });

            let diagnostic = match (median, passed) {
                (Some(m), true) => {
                    let dir_label = match direction {
                        TokenMetricDirection::LowerIsBetter => "lower is better",
                        TokenMetricDirection::HigherIsBetter => "higher is better",
                    };
                    format!("`{metric_name}` median {m:.2} ({dir_label}) — present")
                }
                (Some(m), false) => {
                    format!("`{metric_name}` median {m:.2} is zero or negative — no signal")
                }
                (None, _) => {
                    format!(
                        "`{metric_name}` has fewer than {MIN_TOKEN_GATE_SAMPLES} values across {sample_count} samples"
                    )
                }
            };

            if !passed {
                surface_pass = false;
            }

            metric_evaluations.push(TokenSurfaceMetricEvaluation {
                metric: metric_name.to_string(),
                direction,
                baseline_median: None,
                candidate_median: median,
                passed,
                diagnostic,
            });
        }

        if !surface_pass {
            any_block = true;
            top_diagnostics.push(format!("`{display}`: metric regression"));
        }

        surface_evaluations.push(TokenSurfaceEvaluation {
            surface: surface.to_string(),
            display_name: display,
            sample_count,
            verdict: if surface_pass {
                TokenSurfaceVerdict::Pass
            } else {
                TokenSurfaceVerdict::Regressed
            },
            metric_evaluations,
            diagnostics: Vec::new(),
        });
    }

    TokenGateReport {
        min_samples: MIN_TOKEN_GATE_SAMPLES,
        allowed_regression_percent,
        surface_evaluations,
        decision: if any_block {
            TokenGateDecision::Block
        } else {
            TokenGateDecision::Pass
        },
        diagnostics: top_diagnostics,
    }
}

pub fn evaluate_token_regression(
    baseline: &[TokenGateSample],
    candidate: &[TokenGateSample],
    allowed_regression_percent: f64,
) -> TokenGateReport {
    let mut surface_evaluations = Vec::with_capacity(TOKEN_GATE_SURFACES.len());
    let mut top_diagnostics = Vec::new();
    let mut any_block = false;

    for surface in TOKEN_GATE_SURFACES {
        let display = surface_display_name(surface).to_string();
        let baseline_samples: Vec<&TokenGateSample> =
            baseline.iter().filter(|s| s.surface == surface).collect();
        let candidate_samples: Vec<&TokenGateSample> =
            candidate.iter().filter(|s| s.surface == surface).collect();

        if baseline_samples.is_empty() && candidate_samples.is_empty() {
            surface_evaluations.push(TokenSurfaceEvaluation {
                surface: surface.to_string(),
                display_name: display.clone(),
                sample_count: 0,
                verdict: TokenSurfaceVerdict::Missing,
                metric_evaluations: Vec::new(),
                diagnostics: vec![format!(
                    "surface `{display}` has no baseline or candidate samples"
                )],
            });
            any_block = true;
            top_diagnostics.push(format!("`{display}`: missing"));
            continue;
        }

        if baseline_samples.len() < MIN_TOKEN_GATE_SAMPLES
            || candidate_samples.len() < MIN_TOKEN_GATE_SAMPLES
        {
            let b_count = baseline_samples.len();
            let c_count = candidate_samples.len();
            surface_evaluations.push(TokenSurfaceEvaluation {
                surface: surface.to_string(),
                display_name: display.clone(),
                sample_count: b_count.max(c_count),
                verdict: TokenSurfaceVerdict::InsufficientSamples,
                metric_evaluations: Vec::new(),
                diagnostics: vec![format!(
                    "surface `{display}` baseline={b_count} candidate={c_count}; gate requires {MIN_TOKEN_GATE_SAMPLES} each"
                )],
            });
            any_block = true;
            top_diagnostics.push(format!(
                "`{display}`: insufficient samples (baseline={b_count}, candidate={c_count})"
            ));
            continue;
        }

        let mut metric_evaluations = Vec::with_capacity(REQUIRED_TOKEN_METRICS.len());
        let mut diagnostics = Vec::new();
        let mut surface_pass = true;
        let regression_multiplier = allowed_regression_percent / 100.0;

        for metric_name in REQUIRED_TOKEN_METRICS {
            let baseline_values: Vec<f64> = baseline_samples
                .iter()
                .filter_map(|s| s.metrics.get(metric_name).copied())
                .collect();
            let candidate_values: Vec<f64> = candidate_samples
                .iter()
                .filter_map(|s| s.metrics.get(metric_name).copied())
                .collect();
            let direction = metric_direction(metric_name);
            let baseline_median = if baseline_values.len() >= MIN_TOKEN_GATE_SAMPLES {
                median_f64(&baseline_values)
            } else {
                None
            };
            let candidate_median = if candidate_values.len() >= MIN_TOKEN_GATE_SAMPLES {
                median_f64(&candidate_values)
            } else {
                None
            };

            let (passed, diagnostic) = match (baseline_median, candidate_median) {
                (Some(base), Some(cand)) => {
                    let ok = match direction {
                        TokenMetricDirection::LowerIsBetter => {
                            cand <= base * (1.0 + regression_multiplier)
                        }
                        TokenMetricDirection::HigherIsBetter => {
                            cand >= base * (1.0 - regression_multiplier)
                        }
                    };
                    let diagnostic = if ok {
                        format!(
                            "`{metric_name}`: candidate {cand:.2} vs baseline {base:.2} ({}) — within budget",
                            match direction {
                                TokenMetricDirection::LowerIsBetter => "lower is better",
                                TokenMetricDirection::HigherIsBetter => "higher is better",
                            }
                        )
                    } else {
                        format!(
                            "`{metric_name}` REGRESSES: candidate {cand:.2} vs baseline {base:.2} ({})",
                            match direction {
                                TokenMetricDirection::LowerIsBetter => "lower is better",
                                TokenMetricDirection::HigherIsBetter => "higher is better",
                            }
                        )
                    };
                    (ok, diagnostic)
                }
                (Some(_), None) => (
                    false,
                    format!(
                        "`{metric_name}`: candidate has fewer than {MIN_TOKEN_GATE_SAMPLES} values"
                    ),
                ),
                (None, Some(_)) => (
                    false,
                    format!(
                        "`{metric_name}`: baseline has fewer than {MIN_TOKEN_GATE_SAMPLES} values"
                    ),
                ),
                (None, None) => (
                    false,
                    format!(
                        "`{metric_name}`: neither baseline nor candidate has {MIN_TOKEN_GATE_SAMPLES} values"
                    ),
                ),
            };

            if !passed {
                surface_pass = false;
            }

            diagnostics.push(diagnostic.clone());
            metric_evaluations.push(TokenSurfaceMetricEvaluation {
                metric: metric_name.to_string(),
                direction,
                baseline_median,
                candidate_median,
                passed,
                diagnostic,
            });
        }

        if !surface_pass {
            any_block = true;
            top_diagnostics.push(format!("`{display}`: regression detected"));
        }

        surface_evaluations.push(TokenSurfaceEvaluation {
            surface: surface.to_string(),
            display_name: display,
            sample_count: candidate_samples.len(),
            verdict: if surface_pass {
                TokenSurfaceVerdict::Pass
            } else {
                TokenSurfaceVerdict::Regressed
            },
            metric_evaluations,
            diagnostics,
        });
    }

    TokenGateReport {
        min_samples: MIN_TOKEN_GATE_SAMPLES,
        allowed_regression_percent,
        surface_evaluations,
        decision: if any_block {
            TokenGateDecision::Block
        } else {
            TokenGateDecision::Pass
        },
        diagnostics: top_diagnostics,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
    ) -> Value {
        let mut metrics = serde_json::Map::new();
        metrics.insert("prompt_tokens".into(), Value::from(prompt_tokens));
        metrics.insert("envelope_bytes".into(), Value::from(envelope_bytes));
        metrics.insert("runtime_micros".into(), Value::from(runtime_micros));
        metrics.insert("cache_hit_rate_percent".into(), Value::from(cache_hit_rate));
        metrics.insert("raw_read_avoidance".into(), Value::from(raw_read_avoidance));
        metrics.insert("useful_hit_density".into(), Value::from(useful_hit_density));
        let mut entry = serde_json::Map::new();
        entry.insert(
            "label".into(),
            Value::from(format!("synth {surface} sample")),
        );
        entry.insert("id".into(), Value::from(id.to_string()));
        entry.insert("timestamp".into(), Value::from("2026-06-02T00:00:00Z"));
        entry.insert("surface".into(), Value::from(surface.to_string()));
        entry.insert("metrics".into(), Value::Object(metrics));
        Value::Object(entry)
    }

    fn build_token_history(samples: Vec<Value>) -> String {
        let mut root = serde_json::Map::new();
        root.insert("entries".into(), Value::Array(samples));
        Value::Object(root).to_string()
    }

    fn full_token_history_three_samples_each() -> String {
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
        build_token_history(entries)
    }

    #[test]
    fn parse_token_history_extracts_surfaces_and_metrics() {
        let raw = synth_token_sample(
            "test-cp-2026-06-02-sample-1",
            "context_pack",
            100.0,
            512.0,
            50_000.0,
            90.0,
            8.0,
            0.85,
        );
        let history_raw = build_token_history(vec![raw]);
        let samples = parse_token_history(&history_raw).unwrap();
        assert_eq!(samples.len(), 1);
        assert_eq!(samples[0].surface, "context_pack");
        assert_eq!(samples[0].metrics.len(), 6);
    }

    #[test]
    fn token_gate_passes_when_all_surfaces_have_samples_with_signal() {
        let raw = full_token_history_three_samples_each();
        let history = parse_token_history(&raw).unwrap();
        let report = evaluate_token_gate(&history, 10.0);
        assert_eq!(report.decision, TokenGateDecision::Pass, "{report:?}");
        assert!(
            report
                .surface_evaluations
                .iter()
                .all(|s| s.verdict == TokenSurfaceVerdict::Pass)
        );
    }

    #[test]
    fn token_gate_blocks_when_surface_is_missing() {
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
    fn token_gate_blocks_when_insufficient_samples() {
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
        assert!(
            report
                .surface_evaluations
                .iter()
                .all(|s| s.verdict == TokenSurfaceVerdict::InsufficientSamples)
        );
    }

    #[test]
    fn token_regression_passes_when_candidate_matches_baseline() {
        let raw = full_token_history_three_samples_each();
        let baseline = parse_token_history(&raw).unwrap();
        let candidate = baseline.clone();
        let report = evaluate_token_regression(&baseline, &candidate, 10.0);
        assert_eq!(report.decision, TokenGateDecision::Pass, "{report:?}");
    }

    #[test]
    fn token_regression_blocks_when_lower_is_better_metric_regresses() {
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
        let baseline = parse_token_history(&build_token_history(baseline_entries)).unwrap();
        let candidate = parse_token_history(&build_token_history(candidate_entries)).unwrap();
        let report = evaluate_token_regression(&baseline, &candidate, 10.0);
        assert_eq!(report.decision, TokenGateDecision::Block);
        assert!(report.surface_evaluations.iter().all(|s| {
            s.metric_evaluations
                .iter()
                .find(|m| m.metric == "prompt_tokens")
                .is_some_and(|m| !m.passed)
        }));
    }

    #[test]
    fn token_regression_blocks_when_higher_is_better_metric_regresses() {
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
        let baseline = parse_token_history(&build_token_history(baseline_entries)).unwrap();
        let candidate = parse_token_history(&build_token_history(candidate_entries)).unwrap();
        let report = evaluate_token_regression(&baseline, &candidate, 10.0);
        assert_eq!(report.decision, TokenGateDecision::Block);
    }

    #[test]
    fn token_regression_blocks_when_both_missing() {
        let baseline = parse_token_history(&build_token_history(vec![])).unwrap();
        let candidate = parse_token_history(&build_token_history(vec![])).unwrap();
        let report = evaluate_token_regression(&baseline, &candidate, 10.0);
        assert_eq!(report.decision, TokenGateDecision::Block);
        assert!(
            report
                .surface_evaluations
                .iter()
                .all(|s| s.verdict == TokenSurfaceVerdict::Missing)
        );
    }
}
