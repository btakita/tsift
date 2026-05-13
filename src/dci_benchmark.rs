use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};

const EXPECTED_STRATEGIES: [&str; 3] = ["exact_chained_rg", "lexical_bm25", "hybrid"];

#[derive(Debug, Clone, Deserialize)]
pub struct DciBenchmarkFixture {
    #[serde(default)]
    pub description: Option<String>,
    pub tasks: Vec<DciBenchmarkTask>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct DciBenchmarkTask {
    pub id: String,
    #[serde(default)]
    pub label: Option<String>,
    #[serde(default)]
    pub target: Option<String>,
    pub runs: Vec<DciBenchmarkRun>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct DciBenchmarkRun {
    pub strategy: String,
    pub localized: bool,
    pub tool_calls: f64,
    pub latency_ms: f64,
    pub estimated_tokens: f64,
    #[serde(default)]
    pub notes: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct DciBenchmarkReport {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    pub tasks_loaded: usize,
    pub strategies_compared: usize,
    pub expected_strategies: Vec<String>,
    pub strategy_summaries: Vec<DciStrategySummary>,
    pub task_rows: Vec<DciTaskRow>,
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub warnings: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct DciStrategySummary {
    pub strategy: String,
    pub task_runs: usize,
    pub localized: usize,
    pub localization_rate: f64,
    pub avg_tool_calls: f64,
    pub avg_latency_ms: f64,
    pub avg_estimated_tokens: f64,
    pub rank: usize,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct DciTaskRow {
    pub task_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub target: Option<String>,
    pub best_localization: Vec<String>,
    pub lowest_tool_calls: Option<String>,
    pub lowest_latency: Option<String>,
    pub lowest_token_budget: Option<String>,
}

#[derive(Default)]
struct Accumulator {
    task_runs: usize,
    localized: usize,
    tool_calls: f64,
    latency_ms: f64,
    estimated_tokens: f64,
}

pub fn compute(input: &str) -> Result<DciBenchmarkReport> {
    let fixture: DciBenchmarkFixture =
        serde_json::from_str(input).context("parsing dci-benchmark fixture as JSON")?;
    if fixture.tasks.is_empty() {
        bail!("dci-benchmark fixture did not contain any tasks");
    }

    let mut warnings = Vec::new();
    let mut accumulators = BTreeMap::<String, Accumulator>::new();
    let mut seen_strategies = BTreeSet::<String>::new();
    let mut task_rows = Vec::new();

    for task in &fixture.tasks {
        if task.runs.is_empty() {
            warnings.push(format!(
                "task {} did not include any strategy runs",
                task.id
            ));
            continue;
        }

        let mut localized = Vec::new();
        let mut lowest_tool_calls: Option<&DciBenchmarkRun> = None;
        let mut lowest_latency: Option<&DciBenchmarkRun> = None;
        let mut lowest_tokens: Option<&DciBenchmarkRun> = None;

        for run in &task.runs {
            if !run.tool_calls.is_finite() || run.tool_calls < 0.0 {
                bail!(
                    "task {} strategy {} has invalid tool_calls",
                    task.id,
                    run.strategy
                );
            }
            if !run.latency_ms.is_finite() || run.latency_ms < 0.0 {
                bail!(
                    "task {} strategy {} has invalid latency_ms",
                    task.id,
                    run.strategy
                );
            }
            if !run.estimated_tokens.is_finite() || run.estimated_tokens < 0.0 {
                bail!(
                    "task {} strategy {} has invalid estimated_tokens",
                    task.id,
                    run.strategy
                );
            }

            seen_strategies.insert(run.strategy.clone());
            if run.localized {
                localized.push(run.strategy.clone());
            }

            lowest_tool_calls = choose_lowest(lowest_tool_calls, run, |value| value.tool_calls);
            lowest_latency = choose_lowest(lowest_latency, run, |value| value.latency_ms);
            lowest_tokens = choose_lowest(lowest_tokens, run, |value| value.estimated_tokens);

            let acc = accumulators.entry(run.strategy.clone()).or_default();
            acc.task_runs += 1;
            acc.localized += usize::from(run.localized);
            acc.tool_calls += run.tool_calls;
            acc.latency_ms += run.latency_ms;
            acc.estimated_tokens += run.estimated_tokens;
        }

        task_rows.push(DciTaskRow {
            task_id: task.id.clone(),
            label: task.label.clone(),
            target: task.target.clone(),
            best_localization: localized,
            lowest_tool_calls: lowest_tool_calls.map(|run| run.strategy.clone()),
            lowest_latency: lowest_latency.map(|run| run.strategy.clone()),
            lowest_token_budget: lowest_tokens.map(|run| run.strategy.clone()),
        });
    }

    for expected in EXPECTED_STRATEGIES {
        if !seen_strategies.contains(expected) {
            warnings.push(format!("expected strategy {expected} was not present"));
        }
    }

    let mut summaries = accumulators
        .into_iter()
        .map(|(strategy, acc)| {
            let task_runs = acc.task_runs.max(1);
            DciStrategySummary {
                strategy,
                task_runs: acc.task_runs,
                localized: acc.localized,
                localization_rate: acc.localized as f64 / task_runs as f64,
                avg_tool_calls: acc.tool_calls / task_runs as f64,
                avg_latency_ms: acc.latency_ms / task_runs as f64,
                avg_estimated_tokens: acc.estimated_tokens / task_runs as f64,
                rank: 0,
            }
        })
        .collect::<Vec<_>>();
    summaries.sort_by(strategy_rank);
    for (index, summary) in summaries.iter_mut().enumerate() {
        summary.rank = index + 1;
    }

    Ok(DciBenchmarkReport {
        description: fixture.description,
        tasks_loaded: fixture.tasks.len(),
        strategies_compared: summaries.len(),
        expected_strategies: EXPECTED_STRATEGIES
            .iter()
            .map(|strategy| strategy.to_string())
            .collect(),
        strategy_summaries: summaries,
        task_rows,
        warnings,
    })
}

fn choose_lowest<'a, F>(
    current: Option<&'a DciBenchmarkRun>,
    candidate: &'a DciBenchmarkRun,
    metric: F,
) -> Option<&'a DciBenchmarkRun>
where
    F: Fn(&DciBenchmarkRun) -> f64,
{
    match current {
        Some(existing) if metric(existing) <= metric(candidate) => Some(existing),
        _ => Some(candidate),
    }
}

fn strategy_rank(left: &DciStrategySummary, right: &DciStrategySummary) -> std::cmp::Ordering {
    right
        .localization_rate
        .partial_cmp(&left.localization_rate)
        .unwrap_or(std::cmp::Ordering::Equal)
        .then_with(|| {
            left.avg_estimated_tokens
                .partial_cmp(&right.avg_estimated_tokens)
                .unwrap_or(std::cmp::Ordering::Equal)
        })
        .then_with(|| {
            left.avg_tool_calls
                .partial_cmp(&right.avg_tool_calls)
                .unwrap_or(std::cmp::Ordering::Equal)
        })
        .then_with(|| {
            left.avg_latency_ms
                .partial_cmp(&right.avg_latency_ms)
                .unwrap_or(std::cmp::Ordering::Equal)
        })
        .then_with(|| left.strategy.cmp(&right.strategy))
}

pub fn format_number(value: f64) -> String {
    if (value - value.round()).abs() < 0.005 {
        format!("{}", value.round() as i64)
    } else {
        format!("{value:.2}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ranks_localization_then_cost_metrics() {
        let report = compute(
            r#"{
  "tasks": [
    {
      "id": "a",
      "runs": [
        {"strategy": "exact_chained_rg", "localized": true, "tool_calls": 3, "latency_ms": 120, "estimated_tokens": 500},
        {"strategy": "lexical_bm25", "localized": true, "tool_calls": 5, "latency_ms": 800, "estimated_tokens": 900},
        {"strategy": "hybrid", "localized": true, "tool_calls": 4, "latency_ms": 1800, "estimated_tokens": 750}
      ]
    },
    {
      "id": "b",
      "runs": [
        {"strategy": "exact_chained_rg", "localized": true, "tool_calls": 4, "latency_ms": 140, "estimated_tokens": 620},
        {"strategy": "lexical_bm25", "localized": false, "tool_calls": 6, "latency_ms": 900, "estimated_tokens": 1100},
        {"strategy": "hybrid", "localized": true, "tool_calls": 4, "latency_ms": 2100, "estimated_tokens": 820}
      ]
    }
  ]
}"#,
        )
        .unwrap();

        assert_eq!(report.tasks_loaded, 2);
        assert_eq!(report.strategy_summaries[0].strategy, "exact_chained_rg");
        assert_eq!(report.strategy_summaries[0].localized, 2);
        assert_eq!(
            report.task_rows[0].lowest_token_budget.as_deref(),
            Some("exact_chained_rg")
        );
        assert!(report.warnings.is_empty());
    }
}
