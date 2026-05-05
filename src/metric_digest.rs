use anyhow::{Context, Result, bail};
use serde::Serialize;
use serde_json::{Map, Value};
use std::collections::{BTreeMap, BTreeSet};

const EPSILON: f64 = 1e-9;

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

    Ok(MetricDigestReport {
        runs_loaded: all_runs.len(),
        history_runs: history_runs.iter().map(export_run).collect(),
        current_run: export_run(&current),
        previous_run: previous.as_ref().map(export_run),
        shared_metrics: deltas.len(),
        metric_deltas: deltas,
        top_improvements: improvements,
        top_regressions: regressions,
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
        "wins",
    ]
    .iter()
    .any(|needle| normalized.contains(needle))
    {
        return MetricDirection::HigherIsBetter;
    }

    MetricDirection::Unknown
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
}
