use anyhow::{Context, Result, bail};
use serde::Serialize;
use serde_json::{Map, Value};
use std::collections::{BTreeMap, BTreeSet};

const EPSILON: f64 = 1e-9;
const COMMUNITY_SEARCH_PREFIXES: &[&str] = &["communities", "community_search"];
pub const COMMUNITY_SEARCH_WORKLOADS: &[&str] = &["real", "synthetic_multi_module"];
pub const COMMUNITY_SEARCH_REQUIRED_METRICS: &[&str] = &[
    "duration_micros",
    "handle_coverage_pct",
    "stale_behavior_pass",
    "no_tagpath_behavior_pass",
    "duplicate_name_precision",
    "top_community_stability",
];
pub const COMMUNITY_MAX_DURATION_REGRESSION_PERCENT: f64 = 25.0;
pub const COMMUNITY_MIN_HANDLE_COVERAGE_PCT: f64 = 95.0;
pub const COMMUNITY_MIN_DUPLICATE_NAME_PRECISION: f64 = 0.99;
pub const COMMUNITY_MIN_TOP_COMMUNITY_STABILITY: f64 = 0.95;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum MetricDigestTrend {
    Improved,
    Regressed,
    Flat,
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct MetricDigestRun {
    pub label: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub timestamp: Option<String>,
    pub metrics: BTreeMap<String, f64>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct MetricDigestDelta {
    pub metric: String,
    pub current: f64,
    pub previous: f64,
    pub delta: f64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub percent_delta: Option<f64>,
    pub trend: MetricDigestTrend,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CommunitySearchGateDecision {
    Pass,
    Block,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct CommunitySearchWorkloadEvaluation {
    pub workload: String,
    pub status: CommunitySearchGateDecision,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub duration_micros: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub duration_regression_percent: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub handle_coverage_pct: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stale_behavior_pass: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub no_tagpath_behavior_pass: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub duplicate_name_precision: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub top_community_stability: Option<f64>,
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub missing_metrics: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub diagnostics: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct CommunitySearchGateReport {
    pub decision: CommunitySearchGateDecision,
    pub required_workloads: Vec<String>,
    pub required_metrics: Vec<String>,
    pub max_duration_regression_percent: f64,
    pub min_handle_coverage_pct: f64,
    pub min_duplicate_name_precision: f64,
    pub min_top_community_stability: f64,
    pub workloads: Vec<CommunitySearchWorkloadEvaluation>,
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub diagnostics: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct MetricDigestReport {
    pub runs_loaded: usize,
    pub history_runs: Vec<MetricDigestRun>,
    pub current_run: MetricDigestRun,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub previous_run: Option<MetricDigestRun>,
    pub shared_metrics: usize,
    pub metric_deltas: Vec<MetricDigestDelta>,
    pub top_improvements: Vec<MetricDigestDelta>,
    pub top_regressions: Vec<MetricDigestDelta>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub community_search_gate: Option<CommunitySearchGateReport>,
    pub news_table_markdown: String,
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub warnings: Vec<String>,
}

#[derive(Debug, Clone, PartialEq)]
struct InputRun {
    label: String,
    id: Option<String>,
    timestamp: Option<String>,
    metrics: BTreeMap<String, f64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MetricDirection {
    LowerIsBetter,
    HigherIsBetter,
    Unknown,
}

pub fn compute(
    input: &str,
    baseline: Option<&str>,
    selected_metrics: &[String],
    lower_is_better: &[String],
    higher_is_better: &[String],
    history_limit: usize,
    top_limit: usize,
) -> Result<MetricDigestReport> {
    let current_runs = parse_runs(input, "input")?;
    if current_runs.is_empty() {
        bail!("metric-digest input did not contain any runs");
    }

    let baseline_runs = match baseline {
        Some(raw) => parse_runs(raw, "baseline")?,
        None => Vec::new(),
    };

    let current = current_runs
        .last()
        .cloned()
        .context("metric-digest input did not contain a current run")?;
    let previous = if let Some(run) = baseline_runs.last() {
        Some(run.clone())
    } else if current_runs.len() >= 2 {
        current_runs.get(current_runs.len() - 2).cloned()
    } else {
        None
    };

    let all_runs = baseline_runs
        .iter()
        .chain(current_runs.iter())
        .cloned()
        .collect::<Vec<_>>();
    let history_runs = select_history_runs(&all_runs, history_limit);

    let mut warnings = Vec::new();
    let selected = select_metric_keys(&current, previous.as_ref(), selected_metrics, &mut warnings);

    let lower_override = lower_is_better
        .iter()
        .map(|metric| metric.to_ascii_lowercase())
        .collect::<BTreeSet<_>>();
    let higher_override = higher_is_better
        .iter()
        .map(|metric| metric.to_ascii_lowercase())
        .collect::<BTreeSet<_>>();

    let mut deltas = Vec::new();
    if let Some(previous_run) = previous.as_ref() {
        for metric in &selected {
            let Some(&current_value) = current.metrics.get(metric) else {
                continue;
            };
            let Some(&previous_value) = previous_run.metrics.get(metric) else {
                continue;
            };
            let delta = current_value - previous_value;
            let percent_delta = if previous_value.abs() <= EPSILON {
                None
            } else {
                Some((delta / previous_value) * 100.0)
            };
            deltas.push(MetricDigestDelta {
                metric: metric.clone(),
                current: current_value,
                previous: previous_value,
                delta,
                percent_delta,
                trend: classify_trend(metric, delta, &lower_override, &higher_override),
            });
        }
    }

    let mut improvements = deltas
        .iter()
        .filter(|delta| delta.trend == MetricDigestTrend::Improved)
        .cloned()
        .collect::<Vec<_>>();
    improvements.sort_by(metric_delta_rank);
    improvements.truncate(top_limit.max(1));

    let mut regressions = deltas
        .iter()
        .filter(|delta| delta.trend == MetricDigestTrend::Regressed)
        .cloned()
        .collect::<Vec<_>>();
    regressions.sort_by(metric_delta_rank);
    regressions.truncate(top_limit.max(1));

    let community_search_gate = build_community_search_gate(&current, previous.as_ref());
    if matches!(
        community_search_gate.as_ref().map(|gate| gate.decision),
        Some(CommunitySearchGateDecision::Block)
    ) {
        warnings.push("community search gate blocked on missing or regressed metrics".to_string());
    }

    Ok(MetricDigestReport {
        runs_loaded: all_runs.len(),
        history_runs: history_runs.iter().map(export_run).collect(),
        current_run: export_run(&current),
        previous_run: previous.as_ref().map(export_run),
        shared_metrics: deltas.len(),
        metric_deltas: deltas,
        top_improvements: improvements,
        top_regressions: regressions,
        community_search_gate,
        news_table_markdown: build_news_table(&history_runs, &selected),
        warnings,
    })
}

fn parse_runs(input: &str, label: &str) -> Result<Vec<InputRun>> {
    let trimmed = input.trim();
    if trimmed.is_empty() {
        bail!("metric-digest {label} was empty");
    }

    if let Ok(value) = serde_json::from_str::<Value>(trimmed) {
        let mut runs = Vec::new();
        extract_runs_from_value(&value, label, &mut runs)?;
        return Ok(runs);
    }

    let mut runs = Vec::new();
    for (index, line) in trimmed.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let value = serde_json::from_str::<Value>(line)
            .with_context(|| format!("parsing {label} line {} as JSON/NDJSON", index + 1))?;
        extract_runs_from_value(&value, label, &mut runs)?;
    }

    if runs.is_empty() {
        bail!("metric-digest {label} did not contain any runs");
    }
    Ok(runs)
}

fn extract_runs_from_value(value: &Value, source: &str, runs: &mut Vec<InputRun>) -> Result<()> {
    match value {
        Value::Array(items) => {
            for (index, item) in items.iter().enumerate() {
                runs.push(parse_run_object(item, &format!("{source}[{index}]"))?);
            }
            Ok(())
        }
        Value::Object(object) => {
            if let Some(Value::Array(items)) = object.get("runs") {
                for (index, item) in items.iter().enumerate() {
                    runs.push(parse_run_object(item, &format!("{source}.runs[{index}]"))?);
                }
                Ok(())
            } else {
                runs.push(parse_run_object(value, source)?);
                Ok(())
            }
        }
        _ => bail!("metric-digest {source} must be a JSON object, array, or NDJSON object list"),
    }
}

fn parse_run_object(value: &Value, source: &str) -> Result<InputRun> {
    let object = value
        .as_object()
        .with_context(|| format!("metric-digest run {source} must be a JSON object"))?;

    let id = string_field(object, &["id", "run_id"]);
    let timestamp = string_field(object, &["timestamp", "date", "created_at"]);
    let label = string_field(object, &["label", "name", "run"])
        .or_else(|| id.clone())
        .or_else(|| timestamp.clone())
        .unwrap_or_else(|| source.to_string());

    let metrics = if let Some(metrics_value) = object.get("metrics") {
        parse_metrics_object(metrics_value, &format!("{source}.metrics"))?
    } else {
        parse_inline_metrics(object, source)?
    };

    if metrics.is_empty() {
        bail!("metric-digest run {source} did not include any numeric metrics");
    }

    Ok(InputRun {
        label,
        id,
        timestamp,
        metrics,
    })
}

fn parse_metrics_object(value: &Value, source: &str) -> Result<BTreeMap<String, f64>> {
    let object = value
        .as_object()
        .with_context(|| format!("{source} must be a JSON object"))?;
    let mut metrics = BTreeMap::new();
    for (key, value) in object {
        let Some(number) = value.as_f64() else {
            continue;
        };
        metrics.insert(key.clone(), number);
    }
    Ok(metrics)
}

fn parse_inline_metrics(
    object: &Map<String, Value>,
    source: &str,
) -> Result<BTreeMap<String, f64>> {
    let mut metrics = BTreeMap::new();
    for (key, value) in object {
        if matches!(
            key.as_str(),
            "id" | "run_id"
                | "label"
                | "name"
                | "run"
                | "timestamp"
                | "date"
                | "created_at"
                | "metrics"
        ) {
            continue;
        }
        if let Some(number) = value.as_f64() {
            metrics.insert(key.clone(), number);
        }
    }
    if metrics.is_empty() {
        bail!("metric-digest run {source} did not include inline numeric metrics");
    }
    Ok(metrics)
}

fn string_field(object: &Map<String, Value>, candidates: &[&str]) -> Option<String> {
    candidates.iter().find_map(|key| {
        object
            .get(*key)
            .and_then(Value::as_str)
            .map(|value| value.trim().to_string())
            .filter(|value| !value.is_empty())
    })
}

fn select_history_runs(runs: &[InputRun], history_limit: usize) -> Vec<InputRun> {
    let keep = history_limit.max(1).min(runs.len());
    runs[runs.len() - keep..].to_vec()
}

fn select_metric_keys(
    current: &InputRun,
    previous: Option<&InputRun>,
    selected_metrics: &[String],
    warnings: &mut Vec<String>,
) -> Vec<String> {
    if !selected_metrics.is_empty() {
        let mut keys = Vec::new();
        for metric in selected_metrics {
            if current.metrics.contains_key(metric)
                && previous.is_none_or(|run| run.metrics.contains_key(metric))
            {
                keys.push(metric.clone());
            } else {
                warnings.push(format!(
                    "metric `{metric}` was requested but is not present in both compared runs"
                ));
            }
        }
        return keys;
    }

    let mut keys = current.metrics.keys().cloned().collect::<Vec<_>>();
    if let Some(previous_run) = previous {
        keys.retain(|metric| previous_run.metrics.contains_key(metric));
    }
    keys
}

fn classify_trend(
    metric: &str,
    delta: f64,
    lower_override: &BTreeSet<String>,
    higher_override: &BTreeSet<String>,
) -> MetricDigestTrend {
    if delta.abs() <= EPSILON {
        return MetricDigestTrend::Flat;
    }

    match metric_direction(metric, lower_override, higher_override) {
        MetricDirection::LowerIsBetter => {
            if delta < 0.0 {
                MetricDigestTrend::Improved
            } else {
                MetricDigestTrend::Regressed
            }
        }
        MetricDirection::HigherIsBetter => {
            if delta > 0.0 {
                MetricDigestTrend::Improved
            } else {
                MetricDigestTrend::Regressed
            }
        }
        MetricDirection::Unknown => MetricDigestTrend::Unknown,
    }
}

fn metric_direction(
    metric: &str,
    lower_override: &BTreeSet<String>,
    higher_override: &BTreeSet<String>,
) -> MetricDirection {
    let normalized = metric.to_ascii_lowercase();
    if lower_override.contains(&normalized) {
        return MetricDirection::LowerIsBetter;
    }
    if higher_override.contains(&normalized) {
        return MetricDirection::HigherIsBetter;
    }

    if [
        "mae", "mse", "rmse", "loss", "latency", "duration", "time", "cost", "token", "error",
        "stderr", "stddev", "variance", "p95", "p99", "failure", "failures",
    ]
    .iter()
    .any(|needle| normalized.contains(needle))
    {
        return MetricDirection::LowerIsBetter;
    }

    if [
        "score",
        "accuracy",
        "pass",
        "passed",
        "throughput",
        "qps",
        "ops",
        "success",
        "precision",
        "recall",
        "f1",
        "coverage",
        "stability",
        "wins",
    ]
    .iter()
    .any(|needle| normalized.contains(needle))
    {
        return MetricDirection::HigherIsBetter;
    }

    MetricDirection::Unknown
}

fn build_community_search_gate(
    current: &InputRun,
    previous: Option<&InputRun>,
) -> Option<CommunitySearchGateReport> {
    if !has_community_search_metrics(current)
        && previous.is_none_or(|run| !has_community_search_metrics(run))
    {
        return None;
    }

    let mut workloads = Vec::new();
    let mut diagnostics = Vec::new();
    for workload in COMMUNITY_SEARCH_WORKLOADS {
        let evaluation = evaluate_community_search_workload(current, previous, workload);
        diagnostics.extend(
            evaluation
                .diagnostics
                .iter()
                .map(|diagnostic| format!("{workload}: {diagnostic}")),
        );
        workloads.push(evaluation);
    }

    let decision = if workloads
        .iter()
        .all(|workload| workload.status == CommunitySearchGateDecision::Pass)
    {
        CommunitySearchGateDecision::Pass
    } else {
        CommunitySearchGateDecision::Block
    };

    Some(CommunitySearchGateReport {
        decision,
        required_workloads: COMMUNITY_SEARCH_WORKLOADS
            .iter()
            .map(|workload| (*workload).to_string())
            .collect(),
        required_metrics: COMMUNITY_SEARCH_REQUIRED_METRICS
            .iter()
            .map(|metric| (*metric).to_string())
            .collect(),
        max_duration_regression_percent: COMMUNITY_MAX_DURATION_REGRESSION_PERCENT,
        min_handle_coverage_pct: COMMUNITY_MIN_HANDLE_COVERAGE_PCT,
        min_duplicate_name_precision: COMMUNITY_MIN_DUPLICATE_NAME_PRECISION,
        min_top_community_stability: COMMUNITY_MIN_TOP_COMMUNITY_STABILITY,
        workloads,
        diagnostics,
    })
}

fn evaluate_community_search_workload(
    current: &InputRun,
    previous: Option<&InputRun>,
    workload: &str,
) -> CommunitySearchWorkloadEvaluation {
    let duration_micros = community_metric_value(
        &current.metrics,
        workload,
        &["duration_micros", "runtime_micros"],
    );
    let handle_coverage_pct = community_metric_value(
        &current.metrics,
        workload,
        &["handle_coverage_pct", "tagpath_handle_coverage_pct"],
    )
    .map(normalize_percent);
    let stale_behavior_pass = community_metric_value(
        &current.metrics,
        workload,
        &["stale_behavior_pass", "stale_suppression_pass"],
    );
    let no_tagpath_behavior_pass = community_metric_value(
        &current.metrics,
        workload,
        &["no_tagpath_behavior_pass", "no_tagpath_suppression_pass"],
    );
    let duplicate_name_precision = community_metric_value(
        &current.metrics,
        workload,
        &["duplicate_name_precision", "duplicate_tagpath_precision"],
    )
    .map(normalize_ratio);
    let top_community_stability = community_metric_value(
        &current.metrics,
        workload,
        &["top_community_stability", "top_community_jaccard"],
    )
    .map(normalize_ratio);

    let mut missing_metrics = Vec::new();
    let mut diagnostics = Vec::new();
    for metric in COMMUNITY_SEARCH_REQUIRED_METRICS {
        if community_metric_value(&current.metrics, workload, community_metric_aliases(metric))
            .is_none()
        {
            missing_metrics.push((*metric).to_string());
            diagnostics.push(format!("missing metric `{metric}`"));
        }
    }

    let duration_regression_percent = duration_micros.and_then(|current_duration| {
        previous.and_then(|previous_run| {
            community_metric_value(
                &previous_run.metrics,
                workload,
                &["duration_micros", "runtime_micros"],
            )
            .and_then(|previous_duration| {
                if previous_duration.abs() <= EPSILON {
                    None
                } else {
                    Some(((current_duration - previous_duration) / previous_duration) * 100.0)
                }
            })
        })
    });

    if duration_regression_percent
        .is_some_and(|percent| percent > COMMUNITY_MAX_DURATION_REGRESSION_PERCENT)
    {
        diagnostics.push(format!(
            "duration regression exceeds {:.1}% limit",
            COMMUNITY_MAX_DURATION_REGRESSION_PERCENT
        ));
    }
    if handle_coverage_pct.is_some_and(|value| value + EPSILON < COMMUNITY_MIN_HANDLE_COVERAGE_PCT)
    {
        diagnostics.push(format!(
            "handle coverage below {:.1}%",
            COMMUNITY_MIN_HANDLE_COVERAGE_PCT
        ));
    }
    if stale_behavior_pass.is_some_and(|value| value + EPSILON < 1.0) {
        diagnostics.push("stale behavior did not pass".to_string());
    }
    if no_tagpath_behavior_pass.is_some_and(|value| value + EPSILON < 1.0) {
        diagnostics.push("no-tagpath behavior did not pass".to_string());
    }
    if duplicate_name_precision
        .is_some_and(|value| value + EPSILON < COMMUNITY_MIN_DUPLICATE_NAME_PRECISION)
    {
        diagnostics.push(format!(
            "duplicate-name precision below {:.2}",
            COMMUNITY_MIN_DUPLICATE_NAME_PRECISION
        ));
    }
    if top_community_stability
        .is_some_and(|value| value + EPSILON < COMMUNITY_MIN_TOP_COMMUNITY_STABILITY)
    {
        diagnostics.push(format!(
            "top-community stability below {:.2}",
            COMMUNITY_MIN_TOP_COMMUNITY_STABILITY
        ));
    }

    let status = if diagnostics.is_empty() {
        CommunitySearchGateDecision::Pass
    } else {
        CommunitySearchGateDecision::Block
    };

    CommunitySearchWorkloadEvaluation {
        workload: workload.to_string(),
        status,
        duration_micros,
        duration_regression_percent,
        handle_coverage_pct,
        stale_behavior_pass,
        no_tagpath_behavior_pass,
        duplicate_name_precision,
        top_community_stability,
        missing_metrics,
        diagnostics,
    }
}

fn has_community_search_metrics(run: &InputRun) -> bool {
    run.metrics.keys().any(|metric| {
        COMMUNITY_SEARCH_PREFIXES
            .iter()
            .any(|prefix| metric.starts_with(&format!("{prefix}.")))
    })
}

fn community_metric_value(
    metrics: &BTreeMap<String, f64>,
    workload: &str,
    suffixes: &[&str],
) -> Option<f64> {
    for prefix in COMMUNITY_SEARCH_PREFIXES {
        for suffix in suffixes {
            let key = format!("{prefix}.{workload}.{suffix}");
            if let Some(value) = metrics.get(&key) {
                return Some(*value);
            }
        }
    }
    None
}

fn community_metric_aliases(metric: &str) -> &'static [&'static str] {
    match metric {
        "duration_micros" => &["duration_micros", "runtime_micros"],
        "handle_coverage_pct" => &["handle_coverage_pct", "tagpath_handle_coverage_pct"],
        "stale_behavior_pass" => &["stale_behavior_pass", "stale_suppression_pass"],
        "no_tagpath_behavior_pass" => &["no_tagpath_behavior_pass", "no_tagpath_suppression_pass"],
        "duplicate_name_precision" => &["duplicate_name_precision", "duplicate_tagpath_precision"],
        "top_community_stability" => &["top_community_stability", "top_community_jaccard"],
        _ => &[],
    }
}

fn normalize_percent(value: f64) -> f64 {
    if value <= 1.0 { value * 100.0 } else { value }
}

fn normalize_ratio(value: f64) -> f64 {
    if value > 1.0 { value / 100.0 } else { value }
}

fn metric_delta_rank(left: &MetricDigestDelta, right: &MetricDigestDelta) -> std::cmp::Ordering {
    right
        .delta
        .abs()
        .partial_cmp(&left.delta.abs())
        .unwrap_or(std::cmp::Ordering::Equal)
        .then(left.metric.cmp(&right.metric))
}

fn build_news_table(runs: &[InputRun], selected_metrics: &[String]) -> String {
    if runs.is_empty() {
        return String::new();
    }

    let metrics = if selected_metrics.is_empty() {
        runs.last()
            .map(|run| run.metrics.keys().take(6).cloned().collect::<Vec<_>>())
            .unwrap_or_default()
    } else {
        selected_metrics.iter().take(6).cloned().collect::<Vec<_>>()
    };

    if metrics.is_empty() {
        return String::new();
    }

    let mut lines = Vec::new();
    lines.push(format!("| run | {} |", metrics.join(" | ")));
    let divider = std::iter::once("---".to_string())
        .chain(metrics.iter().map(|_| "---:".to_string()))
        .collect::<Vec<_>>();
    lines.push(format!("| {} |", divider.join(" | ")));

    for run in runs {
        let mut row = vec![run.label.clone()];
        for metric in &metrics {
            let value = run
                .metrics
                .get(metric)
                .map(|value| format_number(*value))
                .unwrap_or_else(|| "-".to_string());
            row.push(value);
        }
        lines.push(format!("| {} |", row.join(" | ")));
    }

    lines.join("\n")
}

fn export_run(run: &InputRun) -> MetricDigestRun {
    MetricDigestRun {
        label: run.label.clone(),
        id: run.id.clone(),
        timestamp: run.timestamp.clone(),
        metrics: run.metrics.clone(),
    }
}

pub fn format_number(value: f64) -> String {
    if value.abs() >= 1000.0 {
        return format!("{value:.2}");
    }
    if value.abs() >= 10.0 {
        return format!("{value:.3}");
    }
    if value.abs() >= 1.0 {
        return format!("{value:.4}");
    }
    format!("{value:.5}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compute_compares_latest_run_to_previous_run() {
        let input = r#"{
          "runs": [
            {"label": "day-1", "metrics": {"session_mae": 1.11, "composite_score": 67.5, "cost_usd": 4.2}},
            {"label": "day-2", "metrics": {"session_mae": 1.07, "composite_score": 69.4, "cost_usd": 4.6}}
          ]
        }"#;

        let report = compute(input, None, &[], &[], &[], 3, 3).unwrap();

        assert_eq!(report.runs_loaded, 2);
        assert_eq!(report.current_run.label, "day-2");
        assert_eq!(report.previous_run.as_ref().unwrap().label, "day-1");
        assert_eq!(report.shared_metrics, 3);
        assert!(
            report
                .top_improvements
                .iter()
                .any(|delta| delta.metric == "session_mae")
        );
        assert!(
            report
                .top_regressions
                .iter()
                .any(|delta| delta.metric == "cost_usd")
        );
        assert!(report.news_table_markdown.contains("| run |"));
        assert!(report.news_table_markdown.contains("day-1"));
    }

    #[test]
    fn compute_supports_baseline_file_and_metric_filter() {
        let baseline =
            r#"{"label":"baseline","metrics":{"mae":1.20,"accuracy":0.81,"token_cost":1000}}"#;
        let current =
            r#"{"label":"current","metrics":{"mae":1.05,"accuracy":0.85,"token_cost":1200}}"#;

        let report = compute(
            current,
            Some(baseline),
            &["mae".to_string(), "accuracy".to_string()],
            &[],
            &[],
            2,
            2,
        )
        .unwrap();

        assert_eq!(report.metric_deltas.len(), 2);
        assert!(
            report
                .metric_deltas
                .iter()
                .all(|delta| delta.metric != "token_cost")
        );
        assert_eq!(report.top_improvements.len(), 2);
        assert!(report.top_regressions.is_empty());
    }

    #[test]
    fn parse_runs_accepts_ndjson_and_inline_numeric_fields() {
        let input = r#"{"label":"run-a","session_mae":1.2,"composite_score":66}
{"label":"run-b","session_mae":1.0,"composite_score":68}"#;

        let runs = parse_runs(input, "input").unwrap();

        assert_eq!(runs.len(), 2);
        assert_eq!(runs[0].label, "run-a");
        assert_eq!(runs[1].metrics["session_mae"], 1.0);
    }

    #[test]
    fn metric_direction_infers_common_better_direction_keywords() {
        assert_eq!(
            classify_trend("session_mae", -0.1, &BTreeSet::new(), &BTreeSet::new()),
            MetricDigestTrend::Improved
        );
        assert_eq!(
            classify_trend("accuracy", 0.02, &BTreeSet::new(), &BTreeSet::new()),
            MetricDigestTrend::Improved
        );
        assert_eq!(
            classify_trend("mystery_metric", 1.0, &BTreeSet::new(), &BTreeSet::new()),
            MetricDigestTrend::Unknown
        );
    }

    #[test]
    fn community_search_gate_passes_real_and_synthetic_metrics() {
        let baseline = r#"{
          "label": "community-gate-baseline",
          "metrics": {
            "communities.real.duration_micros": 100000,
            "communities.real.handle_coverage_pct": 96,
            "communities.real.stale_behavior_pass": 1,
            "communities.real.no_tagpath_behavior_pass": 1,
            "communities.real.duplicate_name_precision": 0.99,
            "communities.real.top_community_stability": 0.96,
            "communities.synthetic_multi_module.duration_micros": 180000,
            "communities.synthetic_multi_module.handle_coverage_pct": 97,
            "communities.synthetic_multi_module.stale_behavior_pass": 1,
            "communities.synthetic_multi_module.no_tagpath_behavior_pass": 1,
            "communities.synthetic_multi_module.duplicate_name_precision": 1,
            "communities.synthetic_multi_module.top_community_stability": 0.98
          }
        }"#;
        let current = r#"{
          "label": "community-gate-current",
          "metrics": {
            "communities.real.duration_micros": 108000,
            "communities.real.handle_coverage_pct": 98,
            "communities.real.stale_behavior_pass": 1,
            "communities.real.no_tagpath_behavior_pass": 1,
            "communities.real.duplicate_name_precision": 1,
            "communities.real.top_community_stability": 0.97,
            "communities.synthetic_multi_module.duration_micros": 190000,
            "communities.synthetic_multi_module.handle_coverage_pct": 99,
            "communities.synthetic_multi_module.stale_behavior_pass": 1,
            "communities.synthetic_multi_module.no_tagpath_behavior_pass": 1,
            "communities.synthetic_multi_module.duplicate_name_precision": 1,
            "communities.synthetic_multi_module.top_community_stability": 0.99
          }
        }"#;

        let report = compute(current, Some(baseline), &[], &[], &[], 3, 3).unwrap();
        let gate = report.community_search_gate.unwrap();

        assert_eq!(gate.decision, CommunitySearchGateDecision::Pass);
        assert_eq!(gate.workloads.len(), 2);
        assert!(gate.diagnostics.is_empty());
        assert!(
            report
                .metric_deltas
                .iter()
                .any(|delta| delta.metric == "communities.real.duration_micros"
                    && delta.trend == MetricDigestTrend::Regressed)
        );
    }

    #[test]
    fn community_search_gate_blocks_missing_quality_metrics() {
        let input = r#"{
          "label": "community-gate-current",
          "metrics": {
            "communities.real.duration_micros": 108000,
            "communities.real.handle_coverage_pct": 94,
            "communities.real.stale_behavior_pass": 1,
            "communities.real.duplicate_name_precision": 0.95,
            "communities.real.top_community_stability": 0.97,
            "communities.synthetic_multi_module.duration_micros": 190000,
            "communities.synthetic_multi_module.handle_coverage_pct": 99,
            "communities.synthetic_multi_module.stale_behavior_pass": 1,
            "communities.synthetic_multi_module.no_tagpath_behavior_pass": 1,
            "communities.synthetic_multi_module.duplicate_name_precision": 1,
            "communities.synthetic_multi_module.top_community_stability": 0.99
          }
        }"#;

        let report = compute(input, None, &[], &[], &[], 3, 3).unwrap();
        let gate = report.community_search_gate.unwrap();
        let real = gate
            .workloads
            .iter()
            .find(|workload| workload.workload == "real")
            .unwrap();

        assert_eq!(gate.decision, CommunitySearchGateDecision::Block);
        assert!(
            real.missing_metrics
                .contains(&"no_tagpath_behavior_pass".to_string())
        );
        assert!(
            real.diagnostics
                .iter()
                .any(|diagnostic| diagnostic.contains("handle coverage"))
        );
        assert!(
            report
                .warnings
                .iter()
                .any(|warning| warning.contains("community search gate blocked"))
        );
    }
}
