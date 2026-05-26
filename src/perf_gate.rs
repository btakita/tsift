//! Graph DB performance release gate.
//!
//! Turns repeated `tsift graph-db backend-eval` samples (recorded in
//! `fixtures/graph-db-performance-history.json`) into a binding promotion
//! decision for candidate `GraphStore` backends.
//!
//! Spec: see SPEC.md § "Graph DB Performance Release Gate".
//!
//! Four required workloads (canonical fixture prefix → human gate name):
//!
//! | Fixture metric prefix     | Gate workload name |
//! |---------------------------|--------------------|
//! | `real`                    | `default`          |
//! | `full_projection`         | `full-projection`  |
//! | `synthetic_high_degree`   | `high-degree`      |
//! | `synthetic_deep_chain`    | `deep-chain`       |
//!
//! Promotion rule: a candidate backend (FalkorDB, Kuzu, DuckDB/DuckPGQ,
//! Ladybug, ...) stays blocked until it beats the stabilized SQLite gate on
//! **every** workload across **every** required metric — including
//! `refresh.duration_micros` (projection write cost) and any
//! `lock_wait_micros` / `lock_contention_micros` metric where applicable.
//!
//! At least three samples per workload are required before the gate emits a
//! binding promote/block call.

use anyhow::{Context, Result, bail};
use serde::Serialize;
use serde_json::Value;
use std::collections::BTreeMap;

/// SQLite is the stabilized baseline backend; every candidate must beat it.
pub const BASELINE_BACKEND: &str = "sqlite";

/// Minimum number of samples per workload before the gate's decision is binding.
pub const MIN_SAMPLES_PER_WORKLOAD: usize = 3;

/// User-facing graph path default. Higher hop tiers stay benchmark-only until
/// `evaluate_hop_cap_promotion` returns `Promote`.
pub const HOP_CAP_CURRENT_DEFAULT: usize = 64;

/// Higher hop tiers that backend-eval records as promotion candidates.
pub const HOP_CAP_CANDIDATE_TIERS: [usize; 3] = [128, 256, 512];

/// Workloads that must prove higher hop caps before the default can move.
pub const HOP_CAP_REQUIRED_WORKLOADS: [&str; 3] =
    ["real", "full_projection", "synthetic_deep_chain"];

/// Fixture metric prefixes for the four required gate workloads, in canonical
/// fixture order.
pub const GATE_WORKLOAD_PREFIXES: [&str; 4] = [
    "real",
    "full_projection",
    "synthetic_high_degree",
    "synthetic_deep_chain",
];

/// Mapping from fixture metric prefix to the human-readable gate workload
/// name used in SPEC and operator-facing diagnostics.
pub fn workload_display_name(prefix: &str) -> &'static str {
    match prefix {
        "real" => "default",
        "full_projection" => "full-projection",
        "synthetic_high_degree" => "high-degree",
        "synthetic_deep_chain" => "deep-chain",
        _ => "unknown",
    }
}

/// Per-operation metrics that the gate considers binding on every workload.
/// `refresh.duration_micros` is the projection write cost; SQLite's bundled
/// install and lock behavior cannot be sacrificed for a faster read path on
/// any other operation.
pub const REQUIRED_GATE_METRICS: [&str; 2] = [
    "refresh.duration_micros",
    "total_duration_micros",
];

/// Lock-behavior metrics. If any candidate produces a lock-wait metric on a
/// workload it must also be ≤ SQLite's median for that metric. Absent
/// lock-wait metrics are not by themselves a block — sibling agents are still
/// wiring projection-write lock instrumentation, and the gate refuses to
/// invent evidence.
pub const LOCK_BEHAVIOR_METRIC_SUFFIXES: [&str; 2] = [
    "lock_wait_micros",
    "lock_contention_micros",
];

/// A single fixture run entry, normalized for gate consumption.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct GateSample {
    pub label: String,
    pub id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub timestamp: Option<String>,
    /// Fixture workload prefix discovered in this sample (e.g. `real`,
    /// `full_projection`). A single sample can carry one or more workloads.
    pub workload_prefixes: Vec<String>,
    /// Sample index parsed from the run id (`...sample-3` → `3`). Falls back
    /// to `None` when the id does not encode an index.
    pub sample_index: Option<usize>,
    /// Backends present for each workload prefix (deduplicated, sorted).
    pub backends_by_workload: BTreeMap<String, Vec<String>>,
    /// All numeric metrics, keyed by the original fixture metric key.
    pub metrics: BTreeMap<String, f64>,
}

/// Diagnostic verdict for a single workload.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkloadVerdict {
    /// Candidate beat SQLite by at least `threshold` on every required metric
    /// and matched or beat SQLite on every observed lock-behavior metric.
    Beats,
    /// Candidate failed at least one required metric or lock-behavior metric.
    Regresses,
    /// Fewer than `MIN_SAMPLES_PER_WORKLOAD` samples; insufficient evidence.
    InsufficientSamples,
    /// No samples carried this workload at all.
    Missing,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct WorkloadEvaluation {
    pub workload: String,
    pub display_name: String,
    pub sample_count: usize,
    pub verdict: WorkloadVerdict,
    pub diagnostics: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum GateDecision {
    Promote,
    Block,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct GateReport {
    pub candidate_backend: String,
    pub baseline_backend: String,
    pub min_samples_per_workload: usize,
    pub workload_evaluations: Vec<WorkloadEvaluation>,
    pub decision: GateDecision,
    pub diagnostics: Vec<String>,
}

/// Diagnostic verdict for one workload in the hop-cap promotion gate.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum HopCapWorkloadVerdict {
    /// The candidate hop tier stayed within the allowed latency band and
    /// returned useful path rows for this workload.
    Promotable,
    /// The candidate tier was present but did not satisfy latency or row
    /// usefulness requirements.
    Hold,
    /// Fewer than `MIN_SAMPLES_PER_WORKLOAD` samples; insufficient evidence.
    InsufficientSamples,
    /// No samples carried this workload at all.
    Missing,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct HopCapWorkloadEvaluation {
    pub workload: String,
    pub display_name: String,
    pub sample_count: usize,
    pub verdict: HopCapWorkloadVerdict,
    pub diagnostics: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct HopCapGateReport {
    pub backend: String,
    pub current_default_hops: usize,
    pub candidate_hops: usize,
    pub min_samples_per_workload: usize,
    pub allowed_regression_percent: f64,
    pub required_workloads: Vec<String>,
    pub workload_evaluations: Vec<HopCapWorkloadEvaluation>,
    pub decision: GateDecision,
    pub diagnostics: Vec<String>,
}

/// Parse `fixtures/graph-db-performance-history.json` (or equivalent input)
/// into normalized `GateSample` records.
pub fn parse_history(raw: &str) -> Result<Vec<GateSample>> {
    let value: Value =
        serde_json::from_str(raw).context("perf_gate: failed to parse history JSON")?;
    let runs = match value {
        Value::Object(mut obj) => match obj.remove("runs") {
            Some(Value::Array(arr)) => arr,
            Some(other) => bail!(
                "perf_gate: history `runs` field must be an array, got {}",
                value_type(&other)
            ),
            None => bail!("perf_gate: history JSON object missing `runs` array"),
        },
        Value::Array(arr) => arr,
        other => bail!(
            "perf_gate: history root must be object or array, got {}",
            value_type(&other)
        ),
    };

    let mut samples = Vec::with_capacity(runs.len());
    for (idx, run) in runs.into_iter().enumerate() {
        let obj = run
            .as_object()
            .with_context(|| format!("perf_gate: run #{idx} must be a JSON object"))?;
        let label = obj
            .get("label")
            .and_then(|v| v.as_str())
            .with_context(|| format!("perf_gate: run #{idx} missing string `label`"))?
            .to_string();
        let id = obj
            .get("id")
            .and_then(|v| v.as_str())
            .with_context(|| format!("perf_gate: run #{idx} missing string `id`"))?
            .to_string();
        let timestamp = obj
            .get("timestamp")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());
        let metrics_value = obj
            .get("metrics")
            .with_context(|| format!("perf_gate: run #{idx} missing `metrics` map"))?;
        let metrics_obj = metrics_value
            .as_object()
            .with_context(|| format!("perf_gate: run #{idx} `metrics` must be an object"))?;
        let mut metrics = BTreeMap::new();
        for (key, value) in metrics_obj {
            if let Some(n) = value.as_f64() {
                metrics.insert(key.clone(), n);
            }
        }

        let (workload_prefixes, backends_by_workload) = derive_workloads_and_backends(&metrics);
        let sample_index = parse_sample_index(&id);

        samples.push(GateSample {
            label,
            id,
            timestamp,
            workload_prefixes,
            sample_index,
            backends_by_workload,
            metrics,
        });
    }
    Ok(samples)
}

fn value_type(v: &Value) -> &'static str {
    match v {
        Value::Null => "null",
        Value::Bool(_) => "bool",
        Value::Number(_) => "number",
        Value::String(_) => "string",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
    }
}

fn parse_sample_index(id: &str) -> Option<usize> {
    // Convention: `<scope>-<workload>-<date>-sample-<N>`.
    let tail = id.rsplit("sample-").next()?;
    if tail == id {
        return None;
    }
    tail.parse::<usize>().ok()
}

/// Discover every workload prefix + per-workload backend list present in this
/// metrics map. We look at keys of the form `<workload>.<backend>.<...>`.
fn derive_workloads_and_backends(
    metrics: &BTreeMap<String, f64>,
) -> (Vec<String>, BTreeMap<String, Vec<String>>) {
    let mut by_workload: BTreeMap<String, BTreeMap<String, ()>> = BTreeMap::new();
    for key in metrics.keys() {
        let mut parts = key.splitn(3, '.');
        let workload = match parts.next() {
            Some(w) => w,
            None => continue,
        };
        let backend = match parts.next() {
            Some(b) => b,
            None => continue,
        };
        // Skip workload-summary keys like `full_projection.edges` where the
        // second segment is not a backend id (it has no further `.suffix`).
        if parts.next().is_none() {
            continue;
        }
        if !GATE_WORKLOAD_PREFIXES.contains(&workload) {
            continue;
        }
        by_workload
            .entry(workload.to_string())
            .or_default()
            .insert(backend.to_string(), ());
    }
    let mut workload_prefixes = Vec::with_capacity(by_workload.len());
    let mut backends_by_workload = BTreeMap::new();
    for (workload, backends) in by_workload {
        workload_prefixes.push(workload.clone());
        let mut backend_list: Vec<String> = backends.into_keys().collect();
        backend_list.sort();
        backends_by_workload.insert(workload, backend_list);
    }
    (workload_prefixes, backends_by_workload)
}

/// Compute the per-metric median across a slice of f64 samples. Returns
/// `None` if the slice is empty. Uses the simple "middle element of a sorted
/// copy" definition (averaging the two middle elements for even counts).
fn median(values: &[f64]) -> Option<f64> {
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

/// Collect samples for `workload_prefix` from history, returning per-metric
/// vectors keyed by `(backend, metric_suffix)`.
fn collect_workload_metrics(
    history: &[GateSample],
    workload_prefix: &str,
) -> (
    usize,
    BTreeMap<(String, String), Vec<f64>>,
) {
    let mut sample_count = 0usize;
    let mut per_metric: BTreeMap<(String, String), Vec<f64>> = BTreeMap::new();
    for sample in history {
        if !sample.workload_prefixes.iter().any(|w| w == workload_prefix) {
            continue;
        }
        sample_count += 1;
        let prefix = format!("{workload_prefix}.");
        for (key, value) in &sample.metrics {
            let rest = match key.strip_prefix(&prefix) {
                Some(r) => r,
                None => continue,
            };
            let (backend, suffix) = match rest.split_once('.') {
                Some(s) => s,
                None => continue,
            };
            per_metric
                .entry((backend.to_string(), suffix.to_string()))
                .or_default()
                .push(*value);
        }
    }
    (sample_count, per_metric)
}

fn path_hop_metric_suffix(hops: usize, leaf: &str) -> String {
    if hops == HOP_CAP_CURRENT_DEFAULT {
        format!("path_max_hops.{leaf}")
    } else {
        format!("path_max_hops_{hops}.{leaf}")
    }
}

fn metric_median_with_min_samples(values: &[f64]) -> Option<f64> {
    if values.len() < MIN_SAMPLES_PER_WORKLOAD {
        None
    } else {
        median(values)
    }
}

/// Evaluate whether a measured higher hop tier can replace the user-facing
/// `64`-hop default.
///
/// The gate is intentionally stricter than merely checking that the raw
/// metrics exist. It requires repeated SQLite samples on the real,
/// full-projection, and synthetic deep-chain workloads, keeps the higher tier
/// within the configured latency-regression budget relative to the 64-hop
/// baseline, and proves the higher tier returns useful rows. On the synthetic
/// deep-chain workload, "useful" means the higher cap returns more path rows
/// than the 64-hop cap.
pub fn evaluate_hop_cap_promotion(
    history: &[GateSample],
    candidate_hops: usize,
    allowed_regression_percent: f64,
) -> HopCapGateReport {
    let mut workload_evaluations = Vec::with_capacity(HOP_CAP_REQUIRED_WORKLOADS.len());
    let mut diagnostics = Vec::new();
    let mut any_block = false;

    if candidate_hops <= HOP_CAP_CURRENT_DEFAULT {
        any_block = true;
        diagnostics.push(format!(
            "candidate hop tier {candidate_hops} must be greater than current default {HOP_CAP_CURRENT_DEFAULT}"
        ));
    } else if !HOP_CAP_CANDIDATE_TIERS.contains(&candidate_hops) {
        any_block = true;
        diagnostics.push(format!(
            "candidate hop tier {candidate_hops} is not one of the measured promotion tiers {:?}",
            HOP_CAP_CANDIDATE_TIERS
        ));
    }

    let allowed_multiplier = 1.0 + (allowed_regression_percent / 100.0);
    let baseline_duration_suffix =
        path_hop_metric_suffix(HOP_CAP_CURRENT_DEFAULT, "duration_micros");
    let baseline_rows_suffix = path_hop_metric_suffix(HOP_CAP_CURRENT_DEFAULT, "rows");
    let candidate_duration_suffix = path_hop_metric_suffix(candidate_hops, "duration_micros");
    let candidate_rows_suffix = path_hop_metric_suffix(candidate_hops, "rows");

    for prefix in HOP_CAP_REQUIRED_WORKLOADS {
        let display = workload_display_name(prefix).to_string();
        let (sample_count, per_metric) = collect_workload_metrics(history, prefix);
        let mut workload_diagnostics = Vec::new();

        if sample_count == 0 {
            any_block = true;
            workload_evaluations.push(HopCapWorkloadEvaluation {
                workload: prefix.to_string(),
                display_name: display.clone(),
                sample_count,
                verdict: HopCapWorkloadVerdict::Missing,
                diagnostics: vec![format!(
                    "workload `{display}` has no samples; hop-cap promotion requires {MIN_SAMPLES_PER_WORKLOAD}"
                )],
            });
            diagnostics.push(format!("`{display}`: missing"));
            continue;
        }
        if sample_count < MIN_SAMPLES_PER_WORKLOAD {
            any_block = true;
            workload_evaluations.push(HopCapWorkloadEvaluation {
                workload: prefix.to_string(),
                display_name: display.clone(),
                sample_count,
                verdict: HopCapWorkloadVerdict::InsufficientSamples,
                diagnostics: vec![format!(
                    "workload `{display}` has {sample_count} sample(s); hop-cap promotion requires {MIN_SAMPLES_PER_WORKLOAD}"
                )],
            });
            diagnostics.push(format!(
                "`{display}`: only {sample_count}/{MIN_SAMPLES_PER_WORKLOAD} samples"
            ));
            continue;
        }

        let baseline_duration_values = per_metric
            .get(&(
                BASELINE_BACKEND.to_string(),
                baseline_duration_suffix.clone(),
            ))
            .cloned()
            .unwrap_or_default();
        let candidate_duration_values = per_metric
            .get(&(
                BASELINE_BACKEND.to_string(),
                candidate_duration_suffix.clone(),
            ))
            .cloned()
            .unwrap_or_default();
        let baseline_rows_values = per_metric
            .get(&(BASELINE_BACKEND.to_string(), baseline_rows_suffix.clone()))
            .cloned()
            .unwrap_or_default();
        let candidate_rows_values = per_metric
            .get(&(BASELINE_BACKEND.to_string(), candidate_rows_suffix.clone()))
            .cloned()
            .unwrap_or_default();

        let mut verdict = HopCapWorkloadVerdict::Promotable;

        match (
            metric_median_with_min_samples(&baseline_duration_values),
            metric_median_with_min_samples(&candidate_duration_values),
        ) {
            (Some(base), Some(candidate)) => {
                let allowed = base * allowed_multiplier;
                if candidate <= allowed {
                    workload_diagnostics.push(format!(
                        "`{candidate_duration_suffix}` median {candidate:.1}µs ≤ allowed {allowed:.1}µs (64-hop baseline {base:.1}µs)"
                    ));
                } else {
                    verdict = HopCapWorkloadVerdict::Hold;
                    workload_diagnostics.push(format!(
                        "`{candidate_duration_suffix}` REGRESSES: median {candidate:.1}µs > allowed {allowed:.1}µs (64-hop baseline {base:.1}µs)"
                    ));
                }
            }
            (None, Some(_)) => {
                verdict = HopCapWorkloadVerdict::Hold;
                workload_diagnostics.push(format!(
                    "`{baseline_duration_suffix}` has fewer than {MIN_SAMPLES_PER_WORKLOAD} samples"
                ));
            }
            (Some(_), None) => {
                verdict = HopCapWorkloadVerdict::Hold;
                workload_diagnostics.push(format!(
                    "`{candidate_duration_suffix}` has fewer than {MIN_SAMPLES_PER_WORKLOAD} samples"
                ));
            }
            (None, None) => {
                verdict = HopCapWorkloadVerdict::Hold;
                workload_diagnostics.push(format!(
                    "`{baseline_duration_suffix}` and `{candidate_duration_suffix}` have fewer than {MIN_SAMPLES_PER_WORKLOAD} samples"
                ));
            }
        }

        match (
            metric_median_with_min_samples(&baseline_rows_values),
            metric_median_with_min_samples(&candidate_rows_values),
        ) {
            (Some(base_rows), Some(candidate_rows)) if candidate_rows > 0.0 => {
                let useful = if prefix == "synthetic_deep_chain" {
                    candidate_rows > base_rows
                } else {
                    candidate_rows >= base_rows
                };
                if useful {
                    workload_diagnostics.push(format!(
                        "`{candidate_rows_suffix}` median {candidate_rows:.1} row(s) proves useful output against 64-hop baseline {base_rows:.1}"
                    ));
                } else {
                    verdict = HopCapWorkloadVerdict::Hold;
                    workload_diagnostics.push(format!(
                        "`{candidate_rows_suffix}` is not useful: median {candidate_rows:.1} row(s) does not exceed required baseline {base_rows:.1}"
                    ));
                }
            }
            (Some(_), Some(candidate_rows)) => {
                verdict = HopCapWorkloadVerdict::Hold;
                workload_diagnostics.push(format!(
                    "`{candidate_rows_suffix}` is not useful: median {candidate_rows:.1} row(s)"
                ));
            }
            (None, Some(_)) => {
                verdict = HopCapWorkloadVerdict::Hold;
                workload_diagnostics.push(format!(
                    "`{baseline_rows_suffix}` has fewer than {MIN_SAMPLES_PER_WORKLOAD} samples"
                ));
            }
            (Some(_), None) => {
                verdict = HopCapWorkloadVerdict::Hold;
                workload_diagnostics.push(format!(
                    "`{candidate_rows_suffix}` has fewer than {MIN_SAMPLES_PER_WORKLOAD} samples"
                ));
            }
            (None, None) => {
                verdict = HopCapWorkloadVerdict::Hold;
                workload_diagnostics.push(format!(
                    "`{baseline_rows_suffix}` and `{candidate_rows_suffix}` have fewer than {MIN_SAMPLES_PER_WORKLOAD} samples"
                ));
            }
        }

        if verdict != HopCapWorkloadVerdict::Promotable {
            any_block = true;
            diagnostics.push(format!("`{display}`: {candidate_hops}-hop tier held"));
        }

        workload_evaluations.push(HopCapWorkloadEvaluation {
            workload: prefix.to_string(),
            display_name: display,
            sample_count,
            verdict,
            diagnostics: workload_diagnostics,
        });
    }

    HopCapGateReport {
        backend: BASELINE_BACKEND.to_string(),
        current_default_hops: HOP_CAP_CURRENT_DEFAULT,
        candidate_hops,
        min_samples_per_workload: MIN_SAMPLES_PER_WORKLOAD,
        allowed_regression_percent,
        required_workloads: HOP_CAP_REQUIRED_WORKLOADS
            .iter()
            .map(|workload| (*workload).to_string())
            .collect(),
        workload_evaluations,
        decision: if any_block {
            GateDecision::Block
        } else {
            GateDecision::Promote
        },
        diagnostics,
    }
}

/// Evaluate the promotion gate for a candidate backend against the baseline
/// (SQLite). `improvement_threshold` is the multiplicative improvement
/// required for each required metric (e.g. `0.0` accepts parity, `0.05`
/// requires the candidate to be ≥ 5% faster than SQLite's median).
pub fn evaluate_promotion(
    history: &[GateSample],
    candidate_backend: &str,
    improvement_threshold: f64,
) -> GateReport {
    let mut workload_evaluations = Vec::with_capacity(GATE_WORKLOAD_PREFIXES.len());
    let mut top_level_diagnostics = Vec::new();
    let mut any_block = false;

    for prefix in GATE_WORKLOAD_PREFIXES {
        let display = workload_display_name(prefix).to_string();
        let (sample_count, per_metric) = collect_workload_metrics(history, prefix);
        let mut diagnostics = Vec::new();

        if sample_count == 0 {
            workload_evaluations.push(WorkloadEvaluation {
                workload: prefix.to_string(),
                display_name: display.clone(),
                sample_count,
                verdict: WorkloadVerdict::Missing,
                diagnostics: vec![format!(
                    "workload `{display}` has no samples in history; gate blocks until at least {MIN_SAMPLES_PER_WORKLOAD} samples are recorded"
                )],
            });
            any_block = true;
            top_level_diagnostics.push(format!("`{display}`: missing"));
            continue;
        }
        if sample_count < MIN_SAMPLES_PER_WORKLOAD {
            workload_evaluations.push(WorkloadEvaluation {
                workload: prefix.to_string(),
                display_name: display.clone(),
                sample_count,
                verdict: WorkloadVerdict::InsufficientSamples,
                diagnostics: vec![format!(
                    "workload `{display}` has {sample_count} sample(s); gate requires {MIN_SAMPLES_PER_WORKLOAD}"
                )],
            });
            any_block = true;
            top_level_diagnostics.push(format!(
                "`{display}`: only {sample_count}/{MIN_SAMPLES_PER_WORKLOAD} samples"
            ));
            continue;
        }

        // Verify the required metrics + lock-behavior metrics for this candidate.
        let mut verdict = WorkloadVerdict::Beats;
        for metric_suffix in REQUIRED_GATE_METRICS {
            let baseline_values = per_metric
                .get(&(BASELINE_BACKEND.to_string(), metric_suffix.to_string()))
                .cloned()
                .unwrap_or_default();
            let candidate_values = per_metric
                .get(&(candidate_backend.to_string(), metric_suffix.to_string()))
                .cloned()
                .unwrap_or_default();
            let baseline_median = median(&baseline_values);
            let candidate_median = median(&candidate_values);
            match (baseline_median, candidate_median) {
                (Some(base), Some(cand)) => {
                    // Lower is better for duration metrics. Candidate must be
                    // at most (1 - threshold) * baseline.
                    let allowed = base * (1.0 - improvement_threshold);
                    if cand <= allowed {
                        diagnostics.push(format!(
                            "`{metric_suffix}`: candidate median {cand:.1} ≤ allowed {allowed:.1} (baseline {base:.1})"
                        ));
                    } else {
                        verdict = WorkloadVerdict::Regresses;
                        diagnostics.push(format!(
                            "`{metric_suffix}` REGRESSES: candidate median {cand:.1} > allowed {allowed:.1} (baseline {base:.1})"
                        ));
                    }
                }
                (Some(_), None) => {
                    verdict = WorkloadVerdict::Regresses;
                    diagnostics.push(format!(
                        "`{metric_suffix}`: candidate `{candidate_backend}` produced no samples for this workload"
                    ));
                }
                (None, _) => {
                    verdict = WorkloadVerdict::Regresses;
                    diagnostics.push(format!(
                        "`{metric_suffix}`: baseline `{BASELINE_BACKEND}` produced no samples for this workload"
                    ));
                }
            }
        }

        // Lock-behavior metrics: only enforce when the candidate actually
        // reports them. Missing lock metrics are an instrumentation gap (sibling
        // agents own that work), not a regression on this gate's part.
        for suffix in LOCK_BEHAVIOR_METRIC_SUFFIXES {
            let candidate_values = per_metric
                .get(&(candidate_backend.to_string(), suffix.to_string()))
                .cloned()
                .unwrap_or_default();
            if candidate_values.is_empty() {
                continue;
            }
            let baseline_values = per_metric
                .get(&(BASELINE_BACKEND.to_string(), suffix.to_string()))
                .cloned()
                .unwrap_or_default();
            let baseline_median = median(&baseline_values);
            let candidate_median = median(&candidate_values).unwrap_or(f64::INFINITY);
            match baseline_median {
                Some(base) if candidate_median <= base => {
                    diagnostics.push(format!(
                        "lock metric `{suffix}`: candidate {candidate_median:.1} ≤ baseline {base:.1}"
                    ));
                }
                Some(base) => {
                    verdict = WorkloadVerdict::Regresses;
                    diagnostics.push(format!(
                        "lock metric `{suffix}` REGRESSES: candidate {candidate_median:.1} > baseline {base:.1}"
                    ));
                }
                None => {
                    verdict = WorkloadVerdict::Regresses;
                    diagnostics.push(format!(
                        "lock metric `{suffix}`: candidate reports samples but baseline `{BASELINE_BACKEND}` does not — cannot prove parity"
                    ));
                }
            }
        }

        if verdict != WorkloadVerdict::Beats {
            any_block = true;
            top_level_diagnostics.push(format!("`{display}`: candidate regresses"));
        }
        workload_evaluations.push(WorkloadEvaluation {
            workload: prefix.to_string(),
            display_name: display,
            sample_count,
            verdict,
            diagnostics,
        });
    }

    let decision = if any_block {
        GateDecision::Block
    } else {
        GateDecision::Promote
    };

    GateReport {
        candidate_backend: candidate_backend.to_string(),
        baseline_backend: BASELINE_BACKEND.to_string(),
        min_samples_per_workload: MIN_SAMPLES_PER_WORKLOAD,
        workload_evaluations,
        decision,
        diagnostics: top_level_diagnostics,
    }
}

/// Verdict for the conflict-matrix preparation hotspot regression gate.
///
/// `#gdbprephot`: each tsift release that reduces a preparation hotspot pins
/// the new ceiling here so a later refactor cannot quietly grow the same
/// phase back past its post-fix budget. The gate is fail-closed and refuses
/// to "trust stale ownership" — callers must hand it freshly acquired
/// samples; the gate never caches the previous comparison.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum PreparationHotspotVerdict {
    /// Sample median is at or below the budget ceiling.
    Within,
    /// Sample median exceeded the budget ceiling.
    Regressed,
    /// Fewer than the required sample count was supplied; the gate refuses
    /// to make a binding decision.
    InsufficientSamples,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct PreparationHotspotReport {
    pub phase: String,
    pub min_samples: usize,
    pub sample_count: usize,
    pub budget_micros: u128,
    /// Median across the supplied freshly-acquired samples. `None` when too
    /// few samples were supplied for the gate to compute a median.
    pub observed_median_micros: Option<u128>,
    pub verdict: PreparationHotspotVerdict,
    pub diagnostics: Vec<String>,
}

/// Minimum sample count for the preparation-hotspot regression gate to emit
/// a binding decision (matches the existing backend-eval gate's three-sample
/// median contract).
pub const MIN_HOTSPOT_SAMPLES: usize = 3;

/// Evaluate whether a `conflict_matrix_preparation` phase's freshly observed
/// median exceeds `budget_micros`.
///
/// Callers MUST pass freshly-acquired samples — the gate does not cache or
/// persist prior measurements. This matches `#gdbprephot`'s constraint that
/// the gate "compare freshly-acquired samples, not cached prior-run values".
pub fn evaluate_preparation_hotspot(
    phase: &str,
    samples: &[u128],
    budget_micros: u128,
) -> PreparationHotspotReport {
    let mut diagnostics = Vec::new();
    if samples.len() < MIN_HOTSPOT_SAMPLES {
        diagnostics.push(format!(
            "preparation hotspot `{phase}` needs ≥{MIN_HOTSPOT_SAMPLES} fresh samples; got {}",
            samples.len()
        ));
        return PreparationHotspotReport {
            phase: phase.to_string(),
            min_samples: MIN_HOTSPOT_SAMPLES,
            sample_count: samples.len(),
            budget_micros,
            observed_median_micros: None,
            verdict: PreparationHotspotVerdict::InsufficientSamples,
            diagnostics,
        };
    }
    let mut sorted: Vec<u128> = samples.to_vec();
    sorted.sort_unstable();
    let mid = sorted.len() / 2;
    let observed = if sorted.len().is_multiple_of(2) {
        (sorted[mid - 1] + sorted[mid]) / 2
    } else {
        sorted[mid]
    };
    let verdict = if observed <= budget_micros {
        diagnostics.push(format!(
            "`{phase}` median {observed}µs ≤ budget {budget_micros}µs across {} fresh samples",
            samples.len()
        ));
        PreparationHotspotVerdict::Within
    } else {
        diagnostics.push(format!(
            "`{phase}` REGRESSED: median {observed}µs > budget {budget_micros}µs across {} fresh samples",
            samples.len()
        ));
        PreparationHotspotVerdict::Regressed
    };
    PreparationHotspotReport {
        phase: phase.to_string(),
        min_samples: MIN_HOTSPOT_SAMPLES,
        sample_count: samples.len(),
        budget_micros,
        observed_median_micros: Some(observed),
        verdict,
        diagnostics,
    }
}

/// Static budget for `conflict_matrix_preparation.context_pack_diff` after
/// `#gdbprephot` capped working-tree parsing to the preview budget. The
/// 0.1.48 pre-fix median on agent-loop was ~446 ms; the post-fix three-sample
/// median is ~289 ms (~35 % reduction). We pin the ceiling at 350 ms so
/// modest noise and small repo growth do not flap the gate, while a real
/// regression (`max_parsed_files` removed, unbounded parse re-introduced,
/// per-file `git show HEAD:path` revived for every working-tree change) trips
/// it well before climbing back to the pre-fix ~445 ms band.
pub const CONTEXT_PACK_DIFF_BUDGET_MICROS: u128 = 350_000;

#[cfg(test)]
mod tests {
    use super::*;

    fn synth_sample(
        id: &str,
        workload: &str,
        sample_idx: usize,
        sqlite_us: f64,
        cand_us: f64,
    ) -> Value {
        let mut metrics = serde_json::Map::new();
        metrics.insert(
            format!("{workload}.sqlite.refresh.duration_micros"),
            Value::from(sqlite_us),
        );
        metrics.insert(
            format!("{workload}.sqlite.total_duration_micros"),
            Value::from(sqlite_us * 2.0),
        );
        metrics.insert(
            format!("{workload}.falkordb.refresh.duration_micros"),
            Value::from(cand_us),
        );
        metrics.insert(
            format!("{workload}.falkordb.total_duration_micros"),
            Value::from(cand_us * 2.0),
        );
        let mut run = serde_json::Map::new();
        run.insert(
            "label".into(),
            Value::from(format!("synth {workload} sample {sample_idx}")),
        );
        run.insert("id".into(), Value::from(id.to_string()));
        run.insert("timestamp".into(), Value::from("2026-05-24T00:00:00Z"));
        run.insert("metrics".into(), Value::Object(metrics));
        Value::Object(run)
    }

    fn build_history(samples: Vec<Value>) -> String {
        let mut root = serde_json::Map::new();
        root.insert("runs".into(), Value::Array(samples));
        Value::Object(root).to_string()
    }

    #[test]
    fn parse_history_extracts_workloads_and_sample_index() {
        let raw = build_history(vec![synth_sample(
            "agent-loop-full-projection-2026-05-24-sample-2",
            "full_projection",
            2,
            1000.0,
            500.0,
        )]);
        let samples = parse_history(&raw).unwrap();
        assert_eq!(samples.len(), 1);
        let s = &samples[0];
        assert_eq!(s.workload_prefixes, vec!["full_projection".to_string()]);
        assert_eq!(s.sample_index, Some(2));
        assert!(
            s.backends_by_workload
                .get("full_projection")
                .unwrap()
                .contains(&"sqlite".to_string())
        );
        assert!(
            s.backends_by_workload
                .get("full_projection")
                .unwrap()
                .contains(&"falkordb".to_string())
        );
    }

    fn full_history_three_samples_each(cand_us: f64) -> String {
        let mut runs = Vec::new();
        for prefix in GATE_WORKLOAD_PREFIXES {
            for i in 1..=3 {
                let id = format!("agent-loop-{prefix}-2026-05-24-sample-{i}");
                runs.push(synth_sample(&id, prefix, i, 1000.0, cand_us));
            }
        }
        build_history(runs)
    }

    fn hop_sample(
        id: &str,
        workload: &str,
        sample_idx: usize,
        base_us: f64,
        candidate_us: f64,
        base_rows: f64,
        candidate_rows: f64,
    ) -> Value {
        let mut metrics = serde_json::Map::new();
        metrics.insert(
            format!("{workload}.sqlite.path_max_hops.duration_micros"),
            Value::from(base_us),
        );
        metrics.insert(
            format!("{workload}.sqlite.path_max_hops.rows"),
            Value::from(base_rows),
        );
        metrics.insert(
            format!("{workload}.sqlite.path_max_hops_512.duration_micros"),
            Value::from(candidate_us),
        );
        metrics.insert(
            format!("{workload}.sqlite.path_max_hops_512.rows"),
            Value::from(candidate_rows),
        );
        let mut run = serde_json::Map::new();
        run.insert(
            "label".into(),
            Value::from(format!("hop {workload} sample {sample_idx}")),
        );
        run.insert("id".into(), Value::from(id.to_string()));
        run.insert("timestamp".into(), Value::from("2026-05-26T00:00:00Z"));
        run.insert("metrics".into(), Value::Object(metrics));
        Value::Object(run)
    }

    fn hop_history(
        candidate_us: f64,
        deep_candidate_rows: f64,
        include_full_projection: bool,
    ) -> String {
        let mut runs = Vec::new();
        for workload in HOP_CAP_REQUIRED_WORKLOADS {
            if workload == "full_projection" && !include_full_projection {
                continue;
            }
            for i in 1..=3 {
                let (base_rows, candidate_rows) = if workload == "synthetic_deep_chain" {
                    (65.0, deep_candidate_rows)
                } else {
                    (2.0, 2.0)
                };
                runs.push(hop_sample(
                    &format!("agent-loop-{workload}-hop-2026-05-26-sample-{i}"),
                    workload,
                    i,
                    1000.0,
                    candidate_us,
                    base_rows,
                    candidate_rows,
                ));
            }
        }
        build_history(runs)
    }

    #[test]
    fn evaluate_promotion_blocks_when_candidate_does_not_beat_baseline() {
        let raw = full_history_three_samples_each(2000.0); // candidate slower than sqlite
        let history = parse_history(&raw).unwrap();
        let report = evaluate_promotion(&history, "falkordb", 0.0);
        assert_eq!(report.decision, GateDecision::Block);
        assert!(
            report
                .workload_evaluations
                .iter()
                .all(|w| matches!(w.verdict, WorkloadVerdict::Regresses))
        );
    }

    #[test]
    fn evaluate_promotion_promotes_when_candidate_beats_baseline_on_every_workload() {
        let raw = full_history_three_samples_each(500.0); // candidate 2x faster than sqlite
        let history = parse_history(&raw).unwrap();
        let report = evaluate_promotion(&history, "falkordb", 0.05);
        assert_eq!(
            report.decision,
            GateDecision::Promote,
            "diagnostics: {:?}",
            report.diagnostics
        );
        assert!(
            report
                .workload_evaluations
                .iter()
                .all(|w| matches!(w.verdict, WorkloadVerdict::Beats))
        );
    }

    #[test]
    fn evaluate_promotion_blocks_when_any_workload_has_fewer_than_three_samples() {
        // 3 samples for three workloads, 2 samples for the fourth.
        let mut runs = Vec::new();
        for prefix in ["real", "full_projection", "synthetic_high_degree"] {
            for i in 1..=3 {
                let id = format!("agent-loop-{prefix}-2026-05-24-sample-{i}");
                runs.push(synth_sample(&id, prefix, i, 1000.0, 100.0));
            }
        }
        for i in 1..=2 {
            let id = format!("agent-loop-synthetic_deep_chain-2026-05-24-sample-{i}");
            runs.push(synth_sample(&id, "synthetic_deep_chain", i, 1000.0, 100.0));
        }
        let raw = build_history(runs);
        let history = parse_history(&raw).unwrap();
        let report = evaluate_promotion(&history, "falkordb", 0.0);
        assert_eq!(report.decision, GateDecision::Block);
        let deep_chain = report
            .workload_evaluations
            .iter()
            .find(|w| w.workload == "synthetic_deep_chain")
            .unwrap();
        assert_eq!(deep_chain.verdict, WorkloadVerdict::InsufficientSamples);
        assert_eq!(deep_chain.sample_count, 2);
    }

    #[test]
    fn evaluate_promotion_blocks_when_workload_is_missing() {
        // Only `real` workload — `full_projection`, `synthetic_high_degree`,
        // `synthetic_deep_chain` are absent.
        let mut runs = Vec::new();
        for i in 1..=3 {
            let id = format!("agent-loop-real-2026-05-24-sample-{i}");
            runs.push(synth_sample(&id, "real", i, 1000.0, 100.0));
        }
        let raw = build_history(runs);
        let history = parse_history(&raw).unwrap();
        let report = evaluate_promotion(&history, "falkordb", 0.0);
        assert_eq!(report.decision, GateDecision::Block);
        let missing_count = report
            .workload_evaluations
            .iter()
            .filter(|w| w.verdict == WorkloadVerdict::Missing)
            .count();
        assert_eq!(missing_count, 3);
    }

    #[test]
    fn lock_behavior_metric_blocks_when_candidate_worse_than_baseline() {
        let mut runs = Vec::new();
        for prefix in GATE_WORKLOAD_PREFIXES {
            for i in 1..=3 {
                let id = format!("agent-loop-{prefix}-2026-05-24-sample-{i}");
                let mut metrics = serde_json::Map::new();
                metrics.insert(
                    format!("{prefix}.sqlite.refresh.duration_micros"),
                    Value::from(1000.0),
                );
                metrics.insert(
                    format!("{prefix}.sqlite.total_duration_micros"),
                    Value::from(2000.0),
                );
                metrics.insert(
                    format!("{prefix}.sqlite.lock_wait_micros"),
                    Value::from(10.0),
                );
                metrics.insert(
                    format!("{prefix}.falkordb.refresh.duration_micros"),
                    Value::from(100.0),
                );
                metrics.insert(
                    format!("{prefix}.falkordb.total_duration_micros"),
                    Value::from(200.0),
                );
                metrics.insert(
                    format!("{prefix}.falkordb.lock_wait_micros"),
                    Value::from(5000.0), // candidate has nasty lock contention
                );
                let mut run = serde_json::Map::new();
                run.insert("label".into(), Value::from(format!("lk {prefix} {i}")));
                run.insert("id".into(), Value::from(id));
                run.insert("metrics".into(), Value::Object(metrics));
                runs.push(Value::Object(run));
            }
        }
        let raw = build_history(runs);
        let history = parse_history(&raw).unwrap();
        let report = evaluate_promotion(&history, "falkordb", 0.0);
        assert_eq!(report.decision, GateDecision::Block);
        let regressing = report
            .workload_evaluations
            .iter()
            .filter(|w| matches!(w.verdict, WorkloadVerdict::Regresses))
            .count();
        assert_eq!(regressing, GATE_WORKLOAD_PREFIXES.len());
    }

    #[test]
    fn hop_cap_gate_promotes_when_all_required_workloads_fit_budget() {
        let raw = hop_history(1050.0, 513.0, true);
        let history = parse_history(&raw).unwrap();
        let report = evaluate_hop_cap_promotion(&history, 512, 10.0);
        assert_eq!(report.decision, GateDecision::Promote, "{report:?}");
        assert_eq!(report.current_default_hops, 64);
        assert_eq!(report.candidate_hops, 512);
        assert_eq!(
            report.required_workloads,
            vec![
                "real".to_string(),
                "full_projection".to_string(),
                "synthetic_deep_chain".to_string()
            ]
        );
        assert!(
            report
                .workload_evaluations
                .iter()
                .all(|workload| workload.verdict == HopCapWorkloadVerdict::Promotable)
        );
    }

    #[test]
    fn hop_cap_gate_blocks_when_full_projection_is_missing() {
        let raw = hop_history(900.0, 513.0, false);
        let history = parse_history(&raw).unwrap();
        let report = evaluate_hop_cap_promotion(&history, 512, 10.0);
        assert_eq!(report.decision, GateDecision::Block);
        let full_projection = report
            .workload_evaluations
            .iter()
            .find(|workload| workload.workload == "full_projection")
            .unwrap();
        assert_eq!(full_projection.verdict, HopCapWorkloadVerdict::Missing);
    }

    #[test]
    fn hop_cap_gate_holds_when_candidate_tier_regresses() {
        let raw = hop_history(1500.0, 513.0, true);
        let history = parse_history(&raw).unwrap();
        let report = evaluate_hop_cap_promotion(&history, 512, 10.0);
        assert_eq!(report.decision, GateDecision::Block);
        assert!(report.workload_evaluations.iter().any(|workload| {
            workload
                .diagnostics
                .iter()
                .any(|diagnostic| diagnostic.contains("REGRESSES"))
        }));
    }

    #[test]
    fn hop_cap_gate_blocks_unmeasured_candidate_tier() {
        let raw = hop_history(900.0, 513.0, true);
        let history = parse_history(&raw).unwrap();
        let report = evaluate_hop_cap_promotion(&history, 96, 10.0);
        assert_eq!(report.decision, GateDecision::Block);
        assert!(
            report
                .diagnostics
                .iter()
                .any(|diagnostic| diagnostic.contains("not one of the measured promotion tiers"))
        );
    }

    #[test]
    fn hop_cap_gate_requires_deep_chain_rows_to_expand() {
        let raw = hop_history(900.0, 65.0, true);
        let history = parse_history(&raw).unwrap();
        let report = evaluate_hop_cap_promotion(&history, 512, 10.0);
        assert_eq!(report.decision, GateDecision::Block);
        let deep_chain = report
            .workload_evaluations
            .iter()
            .find(|workload| workload.workload == "synthetic_deep_chain")
            .unwrap();
        assert_eq!(deep_chain.verdict, HopCapWorkloadVerdict::Hold);
        assert!(
            deep_chain
                .diagnostics
                .iter()
                .any(|diagnostic| diagnostic.contains("not useful"))
        );
    }

    // ---- #gdbprephot: preparation hotspot regression gate ----

    #[test]
    fn preparation_hotspot_within_budget_passes() {
        let report = evaluate_preparation_hotspot(
            "conflict_matrix_preparation.context_pack_diff",
            // medians around ~80ms after #gdbprephot fix
            &[60_000, 80_000, 90_000],
            CONTEXT_PACK_DIFF_BUDGET_MICROS,
        );
        assert_eq!(
            report.verdict,
            PreparationHotspotVerdict::Within,
            "{report:?}"
        );
        assert_eq!(report.observed_median_micros, Some(80_000));
        assert_eq!(report.sample_count, 3);
        assert_eq!(report.budget_micros, CONTEXT_PACK_DIFF_BUDGET_MICROS);
    }

    #[test]
    fn preparation_hotspot_over_budget_fails_closed() {
        // Simulate the pre-fix 0.1.48 baseline (~446ms median).
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
    fn preparation_hotspot_with_fewer_than_three_samples_blocks() {
        let report = evaluate_preparation_hotspot(
            "conflict_matrix_preparation.context_pack_diff",
            &[10, 20],
            CONTEXT_PACK_DIFF_BUDGET_MICROS,
        );
        assert_eq!(
            report.verdict,
            PreparationHotspotVerdict::InsufficientSamples
        );
        assert_eq!(report.observed_median_micros, None);
        assert!(report.diagnostics[0].contains("≥3 fresh samples"));
    }

    #[test]
    fn preparation_hotspot_even_sample_count_averages_two_middle_values() {
        // Four samples: median is average of middle two.
        let report = evaluate_preparation_hotspot(
            "conflict_matrix_preparation.context_pack_diff",
            &[100_000, 150_000, 200_000, 250_000],
            CONTEXT_PACK_DIFF_BUDGET_MICROS,
        );
        assert_eq!(report.observed_median_micros, Some(175_000));
        assert_eq!(report.verdict, PreparationHotspotVerdict::Within);
    }

    #[test]
    fn preparation_hotspot_at_exact_budget_passes() {
        let report = evaluate_preparation_hotspot(
            "conflict_matrix_preparation.context_pack_diff",
            &[CONTEXT_PACK_DIFF_BUDGET_MICROS; 3],
            CONTEXT_PACK_DIFF_BUDGET_MICROS,
        );
        assert_eq!(report.verdict, PreparationHotspotVerdict::Within);
    }

    /// Caller must hand over freshly-acquired samples each call: the gate
    /// has no internal state to pollute. This test locks the contract by
    /// running two evaluations in a row and verifying neither result carries
    /// over from the other.
    #[test]
    fn preparation_hotspot_does_not_cache_prior_samples() {
        let fast = evaluate_preparation_hotspot(
            "context_pack_diff",
            &[10_000, 20_000, 30_000],
            CONTEXT_PACK_DIFF_BUDGET_MICROS,
        );
        assert_eq!(fast.verdict, PreparationHotspotVerdict::Within);
        assert_eq!(fast.observed_median_micros, Some(20_000));

        let slow = evaluate_preparation_hotspot(
            "context_pack_diff",
            &[400_000, 500_000, 600_000],
            CONTEXT_PACK_DIFF_BUDGET_MICROS,
        );
        assert_eq!(slow.verdict, PreparationHotspotVerdict::Regressed);
        assert_eq!(slow.observed_median_micros, Some(500_000));
        // Crucially: `slow` did not inherit `fast`'s median; the gate refuses
        // to "trust stale ownership" of prior measurements.
    }
}
